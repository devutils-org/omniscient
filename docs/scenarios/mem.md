# `mem` scenarios — what is worth measuring, and how you know the answer is right

Twenty-three sources, sixteen of which collect in this build, over twenty-three
of their forty backends. What follows is what to ask this family, the session
that asks it, and the property of the workload that makes the answer checkable.

Every number quoted here was read off the tool on the development machine: an
AMD Ryzen 9 3900X (family 17h, twelve cores, 384 KiB of L1d, 6 MiB of L2, 64 MiB
of L3 in four slices), kernel 7.2.6, transparent huge pages set to `always`,
`perf_event_paranoid` at -1, cgroup2 mounted, and `CAP_BPF` and `CAP_PERFMON` on
the binary. Where a number depends on any of that, it says so.

The tests are in `tests/scenario_mem.rs` and the sessions in
`examples/mem-*.yml`. Nothing below is a summary of the tests: a scenario is the
session and the property together, and the test is only where the property is
written down in a form that fails.

---

## The one thing this family gets wrong most often

Six numbers here are all called "memory" and none of them can be substituted for
another. The catalog's cardinal rule is that the metric is always explicit, and
a `mem` request without a named metric fails rather than picking a default. The
scenarios are built the same way: each one says which of the six it is about.

| | What it counts | What it is not |
|---|---|---|
| `mem.rss` | pages in RAM, shared ones counted in full | a number you can add up across processes |
| `mem.pss` | shared pages divided by the number of mappers | a number any single process can be held to |
| `mem.uss` | what dies with the process | what the process is using |
| `mem.heap.live` | what the program asked `malloc` for | what is resident |
| `mem.heap.retained` | pages of ranges the program freed and the allocator kept | RSS minus live heap |
| `mem.cgroup` | what the OOM killer compares against | a footprint at all |

And two that are not quantities of memory in any sense: `mem.pressure` is time
lost because there was not enough, and `mem.faults` is a count of events whose
relationship to an amount of memory is not even monotone. Both are below.

---

## Scenarios

### Does sixty-four megabytes look like sixty-four megabytes?

`examples/mem-footprints.yml` · `sixty_four_megabytes_made_resident_show_up_in_all_three_footprints`

The workload sleeps, allocates a 64 MiB buffer and writes one byte in every page
of it, then sleeps again. The sleeps are not padding: the first gives the run a
baseline taken before anything is allocated, and the second gives it several
polls after the memory exists. The writing is not padding either — `malloc`
moves none of the three metrics, which is the first caveat on `mem.rss` and the
reason an allocation-site profile and an RSS curve never line up.

Sixty-four megabytes written by one process are private to it, so all three
metrics have to see the same rise, and `uss <= pss <= rss` has to hold at every
instant. Measured: a rise of 71.5 MiB resident, 68.4 MiB proportional, 67.1 MiB
private. The excess over 64 MiB is the interpreter growing while it works, which
is why the check is a floor of 64 MiB and a ceiling of twice it rather than an
equality.

The three requests share one `launch`, which is the point. Three runs would be
three processes and three different sixty-four megabytes.

### Do two roads to the same number arrive at it?

`examples/mem-backend-agreement.yml` · `the_two_roads_to_a_footprint_arrive_at_the_same_number`

`--backend force:` bypasses the resolver, which is what makes this a comparison
rather than a measurement, and a session that pins several backends of one
source over one target gets the self-benchmark on standard error: what each cost
and what each reported.

| Source | Backends | Peaks apart |
|---|---|---|
| `mem.pss` | `fs:proc:smaps_rollup` against the per-mapping `smaps` walk | 0.01 % |
| `mem.uss` | the same two | 0.00 % |
| `mem.rss` | `fs:proc:smaps_rollup` against `fs:proc:statm` | 0.00 % on the peak, 6.25 % on the mean |

The first two are the case the catalog exists for: the same fields summed two
ways, one of them reading a block the kernel maintains and the other walking
every mapping in the address space, costing 49 % of the target's cpu against
15 %.

The third is the case class B exists for. The peaks agree exactly and the means
do not, because `statm` publishes one point the rollup does not, and that point
is a fabricated zero for a process that has already exited. See **D1**.

The comparison is on the maxima and not point by point, and that is not
laziness. Reading either file for the first time faults in pages of the reader's
own, so the second read legitimately sees a larger process; two reads are two
instants, and pretending otherwise is what made the first version of this
comparison in `tests/family_mem.rs` fail by 20 %.

### Do the machine's own files and cgroup2 agree?

`examples/mem-machine-counters.yml`, `examples/mem-cgroup-counters.yml` ·
`the_machine_and_the_root_cgroup_are_two_routes_to_one_counter`,
`both_huge_page_backends_read_the_same_promotion_counter`

`mem.pressure` and `mem.swap` each declare their second backend
`equivalent: true` — it degrades nothing, it is the same accounting reached
through a different file. At the root cgroup the two even cover the same tasks,
so the totals have to match, and the test checks *both* against
`/proc/pressure/memory` and `/proc/vmstat`, which it reads itself. Measured: the
same 9 506 373 000 ns of `some` stall and the same 82 711 568 384 bytes swapped
out, through `/proc/pressure/memory` and `memory.pressure`, through
`/proc/vmstat` and `memory.stat`.

Both kernel counters are monotone and the machine is not idle while the suite
runs, so the test reads the kernel before the sessions and again after and
requires the tool's answer to fall inside that bracket. A single read compared
against a single read is two instants pretending to be one, and it made this
test flake once in four full-suite runs before it was bracketed.

The swap assertion is worth stating on its own: `pswpin` and `pswpout` are in
*pages* while almost every neighbouring field in both files is in bytes or
kilobytes, so a collector that read them as anything else would be wrong by
exactly the page size and would look entirely plausible. Checking bytes against
`pswpout * 4096` is what catches that.

`mem.thp` is pinned to both its backends in the same session. They report the
same `promotions_since_boot` — 248 885 — and only `fs:sys` reports `thp_mode`,
which is exactly what the other's `degraded` line declares it loses. A third
field they both carry disagrees by 8 % and nothing declares it: see **D6**.

### Does the tool find the faults the kernel counted?

`examples/mem-faults-ground-truth.yml` · `the_tool_finds_the_faults_the_program_counted_itself`

The kernel keeps a minor-fault count per process and hands it back through
`getrusage`, so the workload prints its own `ru_minflt` on standard error, which
passes straight through the tool. Measured: the kernel counted 1103 minor faults
and the run traced 1087, a shortfall of 1.5 %. Both gaps are in the same
direction and both are startup — the interpreter faults before the collector is
attached, and the program faults between the last drain and its own final read.

What this scenario deliberately does not check is the number of faults against
the amount of memory, and the caveat on the source says why. Touching 256 MiB
cost about eleven hundred faults, not the sixty-five thousand a four-kibibyte
page would predict, because the kernel picks larger folios for larger regions. A
fault count says where faults happened. Dividing it by a page size gives a
quantity of nothing.

### Is a cache miss count a measurement or a number?

`examples/mem-cache-lines.yml` · `a_gibibyte_of_reads_costs_one_l1_miss_per_cache_line`

Almost nothing about a cache counter is portable, and one thing is: arithmetic.
A buffer far larger than any cache, read end to end, has to bring in one line per
sixty-four bytes, and a prefetcher decides when a line arrives, not whether it is
needed.

The workload allocates 128 MiB — twice this machine's whole last-level cache and
four hundred times its L1 — writes a byte in every page so none of it is a
shared zero page, and scans it eight times. That is 1 GiB of reads and
16 777 216 cache lines.

| Counter | Measured | Against |
|---|---|---|
| `mem.cache_l1` | 20 800 503 misses | 16 777 216 lines the scan must bring in |
| `mem.cache_tlb` | 36 411 misses | 262 144 four-kibibyte page crossings |
| `mem.cache_llc` | 1 729 623 misses | not checked; see below |

The L1 count is 1.24 times the line count, and the excess is the interpreter and
the buffer being created. Halving the passes roughly halves the excess: the
marginal cost of three extra passes over 128 MiB was measured at 6 353 378 and
6 255 068 misses in two runs, against the 6 291 456 lines they bring in — within
1.5 % both times. That differential is the tightest ground truth in this family
and it is worth knowing about; the shipped scenario uses the single-run floor
because it costs one launch instead of two.

The TLB count is checked for being *small*. The same gibibyte crosses a quarter
of a million four-kibibyte pages and five hundred and twelve two-mebibyte ones,
so a TLB miss count near the first number would mean the huge pages are not
there. That makes the assertion conditional on the machine's THP mode, which the
test reads from `mem.thp` before believing it — and which is the concrete reason
the caveat says to freeze and log the mode.

### Is an exact number of allocations reported exactly?

`examples/mem-allocation-exact.yml` · `an_exact_number_of_allocations_is_reported_exactly`

Twenty thousand calls to `malloc(4001)`, each freed immediately. 4001 is a size
nothing else in a Python interpreter asks for, so `filter: bytes == 4001` keeps
this program's allocations and no others. Measured: count 20 000, minimum 4001,
maximum 4001, mean 4001, nothing dropped.

Not "about twenty thousand". The shim sees every call, and a count that is short
is a count that lost something and did not say so — which is the next scenario.

### Does the tool say what it did not see?

Three different ways of losing, and the family has all three.

**The shim's pipe.** `examples/mem-allocation-storm.yml` ·
`the_shim_counts_every_record_it_could_not_carry`. The same program with ten
times the work. The shim writes one thirty-two byte record per `malloc` and per
`free` into a non-blocking pipe and never waits for the reader, because stalling
the measured program would be changing it. When the pipe is full the record is
thrown away and counted. So seen plus dropped has to come to 200 000. Measured:
168 962 seen and **7** dropped. See **D2**.

**The freed-range table.** `examples/mem-retained-eviction.yml` ·
`a_freed_range_table_that_overflows_says_how_much_it_dropped`. `mem.heap.retained`
holds 131 072 freed ranges, and the workload frees 140 000 — 8 928 more than the
table holds, so at least that many have to be evicted and said so. Measured:
11 396 dropped, with the reason naming the capacity and the bytes of address
space it stopped watching:

```
LOSS: 11396 dropped, 0 failed reads, 6 late ticks
  1x the freed-range table holds 131072 ranges and dropped the oldest ones, 44613942 bytes
     of address space in all: whatever is still resident in those is missing from the
     retained total
```

This path works, and it only works because the workload paces itself with a
millisecond of sleep every four hundred calls. Without the pacing the shim's
pipe overflows first, the records never reach the table, and the eviction never
happens — which is **D2** hiding a second loss path behind itself.

**A poll that cannot keep up.** `examples/mem-poll-too-fast.yml` ·
`a_poll_faster_than_the_file_can_be_read_counts_its_late_ticks`. Two thousand
anonymous mappings make a `smaps` file with two thousand blocks in it, and the
per-mapping walk is pinned and asked for a thousand times a second. Measured: 43
points over 537 ms where 537 were asked for, and 33 late ticks, each naming how
long the round took and how long it had. A reader who divides that series by the
requested interval is computing a rate over points that were never taken, and
the late ticks are the only warning.

**A target that goes.** `a_target_that_exits_ends_the_run_and_says_why`. The
request asks for five seconds and the program lives a fifth of one. The run
observes 300 ms, says `stopped because process N exited`, and counts no failed
read: a process that dies between two reads is the end of the run, not a lost
sample, and counting it as one would make every clean run look lossy.

### Is the reason the right reason?

`a_request_that_cannot_mean_anything_is_rejected_with_the_reason`,
`a_machine_that_cannot_serve_a_backend_says_so_in_its_own_terms`,
`a_source_this_build_does_not_collect_exits_three_and_says_what_is_missing`,
`the_three_refusals_are_told_apart`

| It says | It means | Exit | The `mem` example |
|---|---|---|---|
| a rejection | the request cannot mean anything | 2 | `mem.bandwidth --pid` |
| no viable backend | this machine cannot serve it | 1 | `mem.cache_llc` pinned to `pmu` on AMD 17h |
| no collector | this build does not implement it | 3 | `mem.reclaim`, and four more |

**Exit 2.** DRAM throughput attributed to one process is not a number that
exists: the memory controller counts what crosses it for the whole package and
has no idea which process asked. The catalog writes the refusal in capitals —
"`--pid` must be REFUSED, never degraded into a system-wide value presented as
per-process attribution" — and the refusal says `the counter is scoped to the
package`, which is the machine's architecture and not a privilege the reader
could go and fix.

This is the one scenario that is **not** in `examples/`, and the reason is
worth recording for the coordinator: `tests/cli.rs::the_shipped_examples_validate`
walks `examples/` and requires every session in it to validate. That is right,
and it is also exactly why an exit-2 scenario cannot live there — a request that
cannot mean anything is precisely a session that does not validate. The session
is written out here instead, and `tests/scenario_mem.rs` feeds it on standard
input:

```yaml
name: DRAM throughput for one process, which does not exist
format: text

requests:
  - label: per-process bandwidth
    source: mem.bandwidth
    target: {pid: 1}
    mode: count
    emit: [rate]
```

```console
$ omn -f - --input-format yaml < that
error: [per-process bandwidth] `mem.bandwidth` cannot be measured per pid: the counter is
       scoped to the package, so no per-process number exists to return
  hint: this source accepts system
$ echo $?
2
```

Four more rejections are checked in the same test, each a different way of
naming something that does not exist: `mem.rss -m count`, `mem.wss -m snapshot`,
`mem.thp --pid` and `mem.rss --system`. The reasons behind them are a footprint
that is polled rather than counted, a working set that is what gets touched over
a window so that a single read observes none, a promotion that cannot be
attributed to a process, and a resident size that is read per process so that a
system target names none. Each refusal ends in a hint naming the modes or the
targets that *would* work, and the test requires that hint to be there.

**Exit 1.** `examples/mem-refused-unavailable.yml`. `mem.cache_llc`'s first
backend names the last-level cache directly and AMD family 17h publishes no
descriptor for it, which the catalog already says. Left alone, the resolver
falls through to the architectural counter and the run succeeds; pinning the
first backend is what reaches the limit. The message sends the reader at the
machine — `check /proc/sys/kernel/perf_event_paranoid, and whether this cpu
exposes the counter at all` — and does not mention a collector. On a machine
with the descriptor this session runs and measures, and the test says so and
skips rather than passing quietly.

**Exit 3.** `examples/mem-refused-not-built.yml`. `mem.reclaim` resolves, it
validates, every axis of it is checked, and nothing collects it: it wants
`bpf:tp`, the classic tracepoint attach, and `src/bpf/` has only
`BPF_RAW_TRACEPOINT_OPEN`. The refusal names the source and the backend, lists
what *does* have a collector, and points at `--dump-request`. The test also runs
`omn validate` on the same file and requires it to pass, because the difference
between "your machine cannot do this" and "we have not written it" is the
difference between an afternoon in a kernel config and a line in this file.

`forcing_a_backend_with_no_collector_exits_three_and_names_it` walks that
systematically: every backend declared in a `mem` chain that has no collector —
`mem.faults` on `bpf:tp`, `mem.heap.alloc` on `bpf:usdt` and `bpf:uprobe`,
`mem.heap.live` on `fs:proc`, `mem.numa` on `pmu`, `mem.wss` on `fs:sys:damon`
and `fs:sys:page_idle` — has to exit 3 and name itself.

### Does every axis the catalog declares work?

`examples/mem-capability-sweep.yml` ·
`every_mode_and_emit_of_the_*_sources_is_served_or_refused_by_the_right_code`,
`the_capability_sweep_session_produces_every_shape_it_asks_for`,
`a_footprint_timeline_reaches_every_format_that_can_carry_it`,
`every_emit_a_mem_source_declares_can_be_produced_by_a_mode_it_declares`

The sweep asks for every mode against every emit, for every `mem` source that
has a collector, and classifies what came back. Exit 0 is served; exit 2 is the
mode-to-emit matrix refusing a shape that mode cannot produce; exit 3 is this
build. **Exit 1 is a lie**, because nothing about this machine is involved in any
of them, and sixteen combinations produce it. See **D5**.

Which sources have a collector is read off `omn list <source>` rather than
listed here, so a source that gains one needs no edit in the test.

The session is the readable half: six sources, one launched program, one window,
every shape the family has a producer for — a series over time, a distribution,
a ranking, a rate, a single number. It is deliberately heterogeneous, because an
emit that renders correctly on a per-tick footprint can still be wrong on a
source whose events arrive in bursts and carry an address.

The format axis is walked separately, on the shape most likely to be piped
somewhere: a `mem.rss` timeline through `json`, `yaml`, `toml`, `ndjson`,
`chrome` and `speedscope`, and then through `folded`, which refuses it by name
rather than approximating.

And one check that needs no machine at all: every emit a source declares has to
be producible by a mode it declares. The mode-to-emit matrix is a property of
the model — `rate` comes out of `count` and `trace`, `timeline` out of `trace`
and `poll` — and a catalog entry that crosses the two lists and finds an emit
unreachable has promised something no build could keep. Two entries in this
family do. See **D3**.

---

## Workloads

Phase two reuses these. Each is a launched program with a property in its source,
and each is written out inside the session that uses it so a reader sees the
number and the measurement together.

| Workload | Guarantees | Where |
|---|---|---|
| **touch sixty-four megabytes** | 64 MiB made resident, a byte written in every page, with a quiet 300 ms before and 400 ms after | `mem-footprints.yml`, `mem-backend-agreement.yml` |
| **the staircase** | five 8 MiB steps, 200 ms apart: 40 MiB in a known shape | `mem-rss-run.yml`, `allocation-cost.yml` |
| **hold much, touch little** | holds 256 MiB resident and walks 16 MiB of it, indefinitely | `mem-working-set.yml` |
| **fragment the arena** | 1024 blocks of 64 KiB, every page written, every other block freed: 32 MiB freed into holes the allocator cannot return | `mem-fragmentation.yml` |
| **an exact allocation count** | exactly N calls to `malloc(4001)`, each freed at once; 4001 is unique to the program in an interpreter, so a filter isolates it | `mem-allocation-exact.yml` (N = 20 000), `mem-allocation-storm.yml` (N = 200 000) |
| **more ranges than the table** | 140 000 blocks allocated then freed, paced so the pipe keeps up | `mem-retained-eviction.yml` |
| **two thousand mappings** | 2000 separate anonymous `mmap`s, each touched: a `smaps` file with 2000 blocks | `mem-poll-too-fast.yml` |
| **a gibibyte of reads** | 128 MiB written page by page, then scanned end to end eight times | `mem-cache-lines.yml` |
| **count your own faults** | touches 256 MiB and prints its own `ru_minflt` on standard error | `mem-faults-ground-truth.yml` |
| **hold, touch, and be counted** | holds 96 MiB, walks 8 MiB of it for 600 ms, allocating throughout | `mem-capability-sweep.yml` |

**What P2 asked for.** "A program that is half on-cpu and half blocked" is not
among these: every `mem` workload is cpu- and memory-bound by construction, and
one that blocked would be measuring the scheduler through a memory source. The
nearest thing this family has is **hold much, touch little**, which is on cpu
continuously and has a working set an order of magnitude below its footprint —
useful to P4 as a program whose memory behaviour is known while something else
is measured about it.

**What `mem` contributes to a crossing, in one line.** `mem` knows *which bytes*
— every fault carries the address that faulted and every allocation carries the
range it owns — and the key that carries it is `mem:[base,len)`. No other family
produces that key. `examples/allocation-cost.yml` is the existing consumer, and
every session in this stream that could carry the key carries it, so P1 can
start from any of them rather than re-running the workload.

---

## What cannot be verified here

Stated rather than hidden, and each with what it would take.

**`mem.cache_llc` has no ground truth on this machine, and no second backend
either.** AMD family 17h publishes no last-level cache descriptor, so the
`pmu` backend that names `LLC-load-misses` cannot be opened at all and the
architectural `PERF_COUNT_HW_CACHE_MISSES` is what answers. That counter saw
1 729 623 misses for a scan that must bring 16 777 216 lines in from DRAM — a
tenth of them — because the prefetcher's fills are not counted as misses by it.
Both halves of what class B wants are therefore unavailable here: there is one
implemented backend rather than two, and the one there is cannot be checked
against the scan. The session is shipped and the scenario is written; on an
Intel machine with the descriptor, `mem-refused-unavailable.yml` runs and the
comparison exists.

**`mem.latency`, `mem.sharing` and `mem.numa`'s `pmu` backend need PEBS.** The
same reason, one layer down: load-latency sampling with a data-source payload,
which nothing in this build decodes. All three are vocabulary and the sweep
skips them because they have no collector at all.

**Swap and pressure are compared where they are quiet.** The two routes agree
exactly at the root cgroup, and on an idle machine that agreement is between two
numbers that are not moving. Making them move needs a cgroup with a `memory.max`
and a process inside it touching more than that — `systemd` delegates enough for
it without privileges, and `tests/family_mem.rs` already does it. A scenario
that forced reclaim on purpose would check the *flow* rather than the stock, and
would be the natural next thing to add here. It is not in this wave: the
apparatus is real work, it belongs beside the one that already exists rather
than duplicated, and what the flow means is already tested there.

**`mem.faults` sampling produces nothing to check.** `mem.faults -m sample@997hz`
over a 30 ms fault burst returned zero events: the period the kernel picks for a
software event at a requested frequency never elapses inside a burst of a
thousand faults. `trace` mode sees every fault and is what the scenario uses.
The sampled path is exercised by the sweep, which only asks that it works.

**Stacks do not reach `mem.heap.alloc` or `mem.faults --mode trace`.** Both
declare the `stack` emit and neither carries frames: `mem.faults` lands stacks
only on the sampling path, and the shim's frames have never been wired through
the stack capture that now exists — `docs/families/mem.md` records that as
blocked work. The run is honest about it in every format that has a place to say
so (`"stacks": [], "unattributed": 1432` in JSON; `folded`, `pprof` and
`speedscope` refuse the run outright), so there is nothing here to fix in the
scenario and nothing yet to check.

**`make check` is clean here, and only on a machine that is not busy.** This is
not a `mem` finding and it is the one most worth passing on, because eight
streams of this wave are running on one machine. Five tests this stream does not
own fail intermittently under load and pass when the machine is quiet:
`tests/budget.rs::the_extra_cpu_a_faster_poll_costs_is_the_extra_cpu_the_shell_observes`,
two in `tests/family_sched.rs`, and
`collect::tests::{the_runner_closes_the_window, what_a_run_reports_spending_is_the_cpu_it_actually_burned}`
in the library. None of them touches `src/`, which this stream does not either,
and one of the failing batches was run with `tests/scenario_mem.rs` removed from
the tree entirely.

How badly it degrades is worth a number. At a load average of 11 on this
twenty-four-cpu machine, `cargo test --test family_sched` alone failed five runs
out of six, and the reason is in the failure itself:

```
3 pinned tasks must each appear once, saw {3813691: …, 3813692: …, 3813693: …, 3813859: …}
```

A fourth task that the test did not launch was scheduled onto the cpu it pinned
three of its own to. The test's ground truth is that cpu 23 belongs to it, and
under a loaded machine it does not. The same shape explains the other four: each
is measuring cpu time or scheduler latency against an assumption of quiet.

Sixteen full-suite runs on a quiet machine produced three such failures; the
last five consecutive runs, with everything in this stream present, were clean,
and the `make check` taken immediately before this stream's commit was clean.
This stream's heaviest scenarios — the eviction workload at 1.4 s and the
gibibyte scan at 0.6 s — add real load of their own, which is worth knowing even
though they did not cause this.

**Session run times.** Most are under a second. Three are not, and each is that
long for a reason that cannot be shrunk without measuring something else:
`mem-retained-eviction.yml` at 1.4 s (280 000 allocator calls is what it costs
to overflow a table of 131 072 ranges), `mem-cache-lines.yml` at 0.6 s (a
gibibyte of reads at DRAM bandwidth), `mem-allocation-storm.yml` at 0.5 s. The
whole of `tests/scenario_mem.rs` is 26 tests in 5.3 s.

---

## Defects found

Seven. Each is reproducible from a session in this repository, each is pinned by
a test that fails when it changes in either direction, and each test's failure
message says what to do when it is fixed. None of them was worked around.

### D1 — `mem.rss` through `fs:proc:statm` invents a zero for a process that has gone

**Class B.** `examples/mem-backend-agreement.yml`, or the two-request session
below.

```yaml
format: text
requests:
  - {label: rollup, source: mem.rss, backend: force:fs:proc:smaps_rollup,
     target: {launch: [python3, -c, "import time; time.sleep(0.3)"]},
     mode: poll@100ms, emit: [timeline]}
  - {label: statm, source: mem.rss, backend: force:fs:proc:statm,
     target: {launch: [python3, -c, "import time; time.sleep(0.3)"]},
     mode: poll@100ms, emit: [timeline]}
```

```
rollup  via fs:proc:smaps_rollup  (mem.rss)
  4 events over 400.40ms, stopped because process 3715118 exited
  timeline
    task:3715118
      +0ns        8.0 KiB
      +100ms      9.4 MiB
      +200ms      9.4 MiB
      +300ms      9.4 MiB

statm  via fs:proc:statm  (mem.rss)
  5 events over 400.40ms, stopped because process 3715118 exited
  timeline
    task:3715118
      +0ns        8.0 KiB
      +100ms      9.4 MiB
      +200ms      9.4 MiB
      +300ms      9.4 MiB
      +400ms      0 B
```

A zombie keeps a `/proc/<pid>/statm`, and it reads `0 0 0 0 0 0 0`; the same
zombie's `smaps_rollup` returns `ESRCH`. So the rollup backend gets
`SampleError::TargetGone` and ends the run, and the `statm` backend parses a
zero and publishes it as a measurement. It is the one answer a footprint must
never invent, and it is the same failure mode `docs/families/mem.md` records for
an `/proc/<pid>/pagemap` descriptor whose memory map is gone: "a dead target
became zero retained memory".

It is not cosmetic, and the self-benchmark is where it shows. On
`examples/mem-backend-agreement.yml` the bench reports the two backends 6.25 %
apart on the value they report — 4.5114e7 against 4.2295e7 — and the whole of
that gap is the zero. Take it out and the `statm` mean is
`4.2295e7 × 16 / 15 = 4.5115e7`, which is the rollup's to five figures. A
comparison bench whose one job is to say whether two roads reach the same number
is being handed a 6 % disagreement manufactured by a value that is not a
measurement.

`read_bytes` in `src/collect/mem/footprint.rs` distinguishes `ENOENT` and
`ESRCH` on the *open*; a zombie's `statm` opens and reads successfully, so the
distinction has to be made on the content. An all-zero `statm` for a process
that had a resident set a tick ago is a target that has gone.

Pinned by `the_two_roads_to_a_footprint_arrive_at_the_same_number`.

### D2 — the shim stops counting its drops as soon as it is dropping

**Class C.** `examples/mem-allocation-storm.yml`. The workload makes exactly
200 000 allocations of 4001 bytes.

```
seen 168962, dropped 7
reasons: the shim could not keep up: allocations happened faster than the pipe drained
```

Seen plus dropped is 168 969. **31 031 records vanished and were not counted.**
A second run: 165 698 seen, 8 dropped, 34 294 uncounted. This breaks the
standing invariant in `CLAUDE.md` — "losses are counted and surfaced to the user,
always" — and it breaks it in the worst available way, by reporting a small
non-zero loss that a reader would take as the whole of it.

The mechanism is in `report()` in `shim/src/lib.rs`:

```rust
let dropped = DROPPED.swap(0, Ordering::Relaxed);
if dropped > 0 && !write_record(fd, OP_DROPPED, std::ptr::null_mut(), dropped) {
    DROPPED.fetch_add(dropped, Ordering::Relaxed);
    return;                      // <- the record this call was for is never counted
}
```

The backlog is restored correctly. The *current* record is not: the early
`return` skips the `write_record` below it and the `DROPPED.fetch_add(1)` that
would have counted it. So the first record of an overflow burst increments the
counter to one, and every record after it while the pipe stays full is discarded
silently. Seven counted drops for thirty-one thousand lost records is seven
bursts.

The fix is one line — count the current record before returning — and the
scenario is how it is checked: `seen + dropped == 200_000`.

The cost is larger than it looks, because this defect hides other loss paths
behind it. The freed-range eviction scenario above — which works correctly —
could not be reached at all until its workload was paced: at full speed the pipe
dropped the records before the table ever saw them, the table never filled, and
the run reported `2 dropped` and no eviction at all.

Pinned by `the_shim_counts_every_record_it_could_not_carry`.

### D3 — two `mem` entries declare an emit no mode they declare can produce

**Class A.** No workload needed; the catalog contradicts itself.

```console
$ omn mem.swap --system -m poll@100ms -e rate -d 200ms
error: [mem.swap #1] mode `poll` cannot produce `rate`
  hint: in `poll` mode the valid emits are raw, timeline, summary
$ omn mem.swap --system -m snapshot -e rate
error: [mem.swap #1] mode `snapshot` cannot produce `rate`
  hint: in `snapshot` mode the valid emits are summary, raw
```

`mem.swap` declares `modes: [poll, snapshot]` and `emits: [timeline, rate, raw,
summary]`. `rate` is producible in `count` and `trace` mode only. So the entry
offers an emit that neither of its own modes can reach, on any machine, in any
build. `mem.sharing` does the same with `summary`, which `sample` — its only
mode — cannot produce.

This is the promise class A is about, broken in the one place nobody looks: a
reader consults `omn list mem.swap`, sees `rate` among the emits, asks for it and
is told no by a rule that lives somewhere else entirely. The refusal is correct
and the exit code is right; the declaration should not have been made.

For `mem.swap` the fix is a judgement the family owns. The source measures a
flow and a rate is the natural thing to want from it, so either the emit goes or
the source gains `count`. For `mem.sharing`, `summary` over a sampled set of
HITM cache lines is close to meaningless and dropping it is the obvious call.

Pinned by `every_emit_a_mem_source_declares_can_be_produced_by_a_mode_it_declares`,
which also checks the matrix it uses against the tool so that the test cannot
pass by being wrong in the same direction twice.

### D4 — a refusal sentence pushed into a slot meant for one word

**Class D.** `omn mem.heap.live --pid 1 -m poll@50ms -e summary`

```
error: `mem.heap.live` cannot be collected for a pid (a preloaded shim can only be given
to a program at exec, so this source needs `-- <program>` rather than an already running
process) target yet
```

The exit code is 3 and that is defensible: the catalog declares `pid: native`
because `bpf:usdt` and `bpf:uprobe` would serve a running process, and neither
is written. The sentence is not. `CollectError::UnsupportedTarget` formats as
``​`{source_id}` cannot be collected for a {target} target yet``, and
`src/collect/mod.rs` fills `{target}` with `format!("{} ({reason})",
target.kind())` where `reason` is a whole sentence from
`Prepared::requires_launch`. The result ends in "...an already running process)
target yet", which reads as a bug in the tool at the moment a reader is already
being told no.

Every family with a launch-only collector reaches this, so it is not a `mem`
fix. The error variant wants a `reason` field of its own.

Pinned by `asking_the_shim_for_a_running_process_is_refused_by_the_build`.

### D5 — sixteen combinations blame the machine for a collector that was not written

**Class A and D together.** The sweep asks for every mode against every emit,
for every `mem` source that collects, and sixteen come back with exit 1.

| Source | Mode | Emits | What it says |
|---|---|---|---|
| `mem.faults` | `count` | `summary`, `rate`, `raw` | ``count` mode does not sample page faults` |
| `mem.cache_l1`, `mem.cache_llc`, `mem.cache_tlb` | `sample` | `topk`, `stack`, `raw` | `needs the sampling path, which lands with stacks; `--mode count` reads the same counter exactly` |
| `mem.heap.live`, `mem.heap.retained` | `snapshot` | `raw`, `summary` | `a snapshot would be taken before it allocated anything; use `--mode poll`` |

Exit 1 means *this machine cannot serve it*. Nothing about this machine is
involved in any of the sixteen. `PERF_COUNT_SW_PAGE_FAULTS` in counting mode is
the most ordinary thing `perf_event_open` does and the counter is right there;
the three cache sources are already counting on the same hardware; the two heap
sources are refusing a shape rather than a capability. Every one of them is
exit 3 — this build has not implemented it — and a reader who takes exit 1 at
its word goes and looks at `perf_event_paranoid`, their kernel config and their
capabilities for something none of those can fix.

Two of the six are arguably catalog errors rather than code gaps.
`mem.heap.live` and `mem.heap.retained` declare `snapshot` for the sake of the
`fs:proc` allocator-stats backend, which no build has; through the shim a
snapshot genuinely cannot mean anything, which would make it exit 2 rather than
either. That ambiguity is itself worth resolving — the message already says
"use `--mode poll`", which is advice for a request that can never work in this
shape.

The messages themselves are good. It is only the code that is wrong, and the
code is what a script reads.

Pinned, per group of sources, by the four
`every_mode_and_emit_of_the_*_sources_is_served_or_refused_by_the_right_code`
tests.

### D6 — the two `mem.thp` backends disagree about resident huge pages, and nothing says so

**Class B.** `examples/mem-machine-counters.yml`, the two `snapshot` requests.

```
fs:sys   :: promotions_since_boot 248885, resident_huge_pages 401, thp_mode 'always'
fs:proc  :: promotions_since_boot 248885, resident_huge_pages 372
```

The promotion counter agrees exactly, which is the comparison the scenario was
written for, and the mode travels on one backend only, which is exactly the
declared degradation. `resident_huge_pages` differs by 8 %, and repeating the
read three times gives 401 against 372 every time: it is systematic, not two
instants.

They are two different kernel counters. `fs:sys` reads `nr_anon` out of
`/sys/kernel/mm/transparent_hugepage/hugepages-2048kB/stats/`; `fs:proc` reads
`nr_anon_transparent_hugepages` out of `/proc/vmstat`, which is the one that
matches `AnonHugePages` in `/proc/meminfo` (761 856 kB / 2 MiB = 372). The same
directory publishes `nr_anon_partially_mapped` at 29, and 372 + 29 = 401.

So the difference is partially-mapped huge pages, and the two backends are
answering slightly different questions under one attribute name. The catalog's
`degraded` line covers the mode and says nothing about this, which is the shape
`docs/contributing.md` warns about: "a run where they disagree is either a bug or
a caveat nobody wrote down". Either the fallback should report the same number,
or `nr_anon_partially_mapped` should travel beside it, or the caveat should be
written.

Pinned by `both_huge_page_backends_read_the_same_promotion_counter`, which
asserts the direction of the difference and says in its message that an equality
means one of them was corrected.

### D7 — a sampling request answered with a single poll, reported as a success

**Class A.** `omn mem.numa -m sample@997hz -e topk -- <a program that touches 32 MiB and sleeps half a second>`

```
mem.numa  via fs:proc  (mem.numa)
  1 events over 1.00s, stopped because the launched program exited with exit status 0
  no loss: every read succeeded and no tick ran late
  degraded: fs:proc: page placement, not accesses: a page on the wrong node that is never
            touched costs nothing, and this cannot tell the two apart

  topk: the 1 heaviest of 1 series
     1. task:3716092    472 count  over 1 events
```

`mem.numa` declares `modes: [sample, poll]`. Its first backend samples memory
accesses through PEBS and is not written; its second reads
`/proc/[pid]/numa_maps`, which is a file, and a file has no sampling rate. The
resolver falls through to it for a `sample@997hz` request, the collector reads
the file once, and the run reports success.

A thousand samples a second were asked for and one read was taken — three orders
of magnitude — and the only thing the output says about a degradation is the
placement-versus-accesses note, which is true, declared in the catalog, and about
something else entirely. `--dump-request` says no more: it shows the chain, the
rejection of `pmu` and the same `degraded` line, and nothing about the mode.

This is the invariant in `CLAUDE.md` — "an invalid combination is refused with
the reason, never degraded silently" — and the reason it matters more than a
missing number is that the number it produces is *plausible*. 472 pages placed
on a node is a real measurement of something; it is simply not a second of
sampling, and nothing in the run distinguishes the two.

`mem.faults` shows what the right answer looks like: it refuses a mode its
backend cannot serve, at attach time, by name. Whether the refusal here should
come from the collector or from the resolver — which knows that the only backend
it has left is poll-only — is a shared-layer question, because every source with
a sampling backend it has not implemented and a polling fallback it has will do
the same thing.

Pinned by
`a_sampling_request_that_no_backend_can_sample_is_not_answered_with_a_poll`,
which accepts either fix: a run that samples, or a run that says which part of
the request it could not honour.

---

## What this leaves for the coordinator

Three things that are not `mem`'s to fix and that this stream found:

- **D2 is a one-line fix in `shim/src/lib.rs` and it should go first.** It
  breaks a standing invariant, it makes every `mem.heap.*` measurement of a busy
  program wrong by an unknown amount, and it masks the loss paths underneath it.
- **D4 and D5 are the refusal vocabulary, not this family.** `UnsupportedTarget`
  wants a `reason` field; a collector refusing a mode it has not implemented
  wants `CollectError::NoCollector` rather than `Start`, so that it exits 3.
- **D7 is the resolver.** A backend that cannot serve the requested mode should
  not be selected silently, and this family is unlikely to be the only one where
  a sampling chain ends in a polling fallback.
- **Five timing-sensitive tests in `budget`, `sched` and the library cannot
  survive eight agents on one machine.** They pass alone and fail together, for
  the reason written out above: their ground truth is an idle cpu. Worth knowing
  before reading any red suite from this wave as a regression.

And one thing about the examples directory: the exit-2 scenario cannot be a file
in `examples/`, because `tests/cli.rs::the_shipped_examples_validate` requires
every session there to validate. That rule is right. If the refusal scenarios are
meant to ship as sessions across all eight families, the rule needs a convention
— a `examples/refused/` subdirectory, or a naming prefix the walk skips — and
that is a coordinator-level decision rather than eight independent ones.
