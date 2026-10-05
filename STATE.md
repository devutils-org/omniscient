# State of the project

Where Omniscient actually is, measured rather than remembered. Every number here
was read off the tool or the suite on the development machine, and every claim
about what is missing names what it is blocked on.

Read `ROADMAP.md` for the plan, `DECISIONS.md` for what is settled,
`docs/open-findings.md` for the findings that cut across families, and this file
for where the two meet.

---

## In one paragraph

The request model, the catalog, the resolver, the validator, the collector
layer, the emit and encode layers and a join engine all exist and are used by
every source. A hundred and fifteen sources across nine families collect for
real, over nine mechanisms: procfs and sysfs, `perf_event_open` counting and sampling,
netlink, an `LD_PRELOAD` shim, NVML through `dlopen`, CUPTI from an injection
library the CUDA driver loads (since the nineteenth wave), `af_packet` with a
`PACKET_MMAP` ring, and eBPF through raw tracepoints, classic tracepoints,
kprobes, uprobes -- and, since the eighth wave, **fentry** and **USDT**, the two
attach kinds eight sources had waited on since the fifth. Every emit the catalog declares exists, all nine output
formats have an encoder, stacks are captured two ways — a `perf_event_open`
callchain for `cpu.profile` and `bpf_get_stackid` for `sched.offcpu`, which is
the one that says *where* a task blocked — and a run measures and reports what
it cost, against a ceiling it can be held to. Since the fourteenth wave a
profile's stacks are unwound by a mode chosen like a backend -- frame pointers,
frame pointers with the leaf's caller recovered, or DWARF from a copied stack,
by machine and target -- and served with user-only stacks at the kernel's
default `perf_event_paranoid`. Since the nineteenth wave `omn top` draws all
of it in a terminal, live or from a recording, through nine views the user
kept after using them; since the twentieth wave they are linked, clickable,
each held on screen to a workload whose load is known in advance, and a tenth,
`process`, is the first screen of a run with a target
(`docs/view/gallery/README.md`).

**All four join match kinds now have a producer**, which took until the sixth
wave. `scoped_eq` was the last, and it was the quarter of the engine that
resolved, validated and had never once been exercised against real events:

```
join queue-then-device  io.queue -> io.latency  evidence KEY
  10553 of 10562 `io.latency` events attributed (99.9 %)
  blk:0xffff8da6271c1200@14446180862290   closed
  blk:0xffff8da6271c1200@14448276351000   closed
```

The same pointer under two open timestamps, four milliseconds apart, is why the
`blk` key is scoped rather than a pointer: the kernel recycles a request within
milliseconds, and a pointer alone would have credited one request's latency to
another.

**2660 tests passed, 0 failed, 13 ignored** across 64 binaries in the
`make check` run after the twentieth wave's matrix sweep (at `3a1aaa1`, green on its first
run, clippy and fmt clean): the first green `make check` since the
seventeenth wave. The wave's last full run before the sweep (`cargo test
--workspace --no-fail-fast` after its merges and the merge's first fix,
`96b4741`) had 2656 passed and 4 failed: three merge regressions where two
streams met, fixed in `d661d67`, and
`portability::every_cell_covers_the_whole_catalog`, whose cells predated
`gpu.profile` until the sweep refreshed them. **The test that was red in every run since the
eighteenth wave is green**:
`runner::a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed`,
since `cpu.profile`'s `losses_by_ring` carries each ring's throttles beside its
drops (`bdef096`). The nineteenth wave's last full run had 2545 passed;
the eighteenth wave's, 2271; the seventeenth wave's last `make check` was green, 2191 passed (`ae889f7`). The ignored are
the bench's measurements, which `scripts/bench.sh` runs on a quiet machine and the suite
never does. Most of them are scenarios: a workload whose property the scenario
chose, a session that measures it, and a check against that property.
Twenty-seven of those are the seventh wave's **crossings** -- a question no
single family can answer, checked against something the workload itself
reports.

**And the engine decomposes now.** One task's wall clock, split by the tool
itself across on a cpu, runnable and blocked, against the program's own life:

```
    on-cpu       cpu.profile        samples      sampled  KEY    198.60ms   +-1.00ms   55.0 %
    runnable     sched.runqlat      total_ns     traced   KEY      1.48ms      exact    0.4 %
    blocked      sched.offcpu       total_ns     traced   KEY    161.68ms      exact   44.8 %
    parts                                                        361.76ms
    whole        the program's life gate to reap lifetime -      361.21ms  +-578.30us
    RESIDUE      whole - parts                                   -550.68us  +-1.58ms
```

The residue is inside its own resolution. Before the eighth wave the same table
had to be built by a test out of JSON, with the run's window as its whole and
the queue wait counted twice, and its residue was +97 ms.
The fourth wave's 63 defects are recorded in `docs/scenarios/`, 52 closed by the
fifth wave in `docs/fixes/`.

**And the vertical case runs** -- `ROADMAP.md` phase 4, the milestone it calls
the real one. A real server on loopback serves requests with delays the test
injected, declares each through USDT, carries its id in the serving thread's
task storage, and the block layer reads it back at issue. Each injected delay
lands in its own row, at `IDENTITY`, with the lock's holder named:

```
  req:a001   lock_wait  40.06ms  IDENTITY  99.9 %  -> holder_tid=1175602
  req:a002   device      8.90ms  IDENTITY  97.8 %  -> dev=nvme0n1p6
```

and the rows sum to the latency the client measured on its own clock within
**+0.11 % to +0.42 %**, against the roadmap's 10 %.

What is missing is breadth, not skeleton, and very little of it is left: **three
sources have no collector**, and each waits on something this machine does not
have or this project has not chosen -- Intel PT on an AMD part, an uncore PMU,
and an XDP program. NVIDIA's CUPTI, the fourth, was served in the nineteenth
wave.

---

## What the numbers say

```
102 sources: 95 usable here, 0 unverified, 7 out of reach on this machine.
99 of them have a collector, covering 112 of their 206 backends.
```

(`target/debug/omn list` on `main` at `95b8b45`, after the twentieth wave's
merges, with the capabilities on the binary; the 112th backend is
`cpu.oncpu_time` through `bpf:raw_tp`, exact per thread. At `c6bd2b8`, after
the nineteenth wave's merges, with `CAP_BPF` and `CAP_PERFMON` on the binary as `getcap` shows them,
at `perf_event_paranoid` -1. The three new are `gpu.kernel`, `gpu.counters`
and the new `gpu.profile`, each through `vendor:cupti`. Before the wave: 101,
92 usable, 96 with a collector and 108 of 205 backends, unchanged since the
fifteenth wave; 56 usable from the same binary relinked without the
capabilities, the matrix's `here-nocaps` cell, swept in the sixteenth wave.
The 108th backend was `cpu.cycles` on `pmu:sw`, the cpu clock as the
fallback for a cpu with no hardware PMU.)

Three different questions, deliberately kept apart.

| Question | Answer | Meaning |
|---|---|---|
| What can this machine serve? | 95 | probed now, **through a backend this build collects**: the file is readable, the event opens, the tracepoint is there |
| What can this build collect? | 99 sources, 112 backends | a collector is written and registered |
| What is neither? | the rest | vocabulary: it resolves, it validates, and it says so |

That `95` is with `CAP_BPF` and `CAP_PERFMON` on the binary, and it is why
`make` reapplies the capabilities that every relink drops. Without them the
whole eBPF and per-cpu perf surface goes, and nothing announces the loss — the
tool reports fewer sources, which reads like the machine changed rather than
the binary.

**That number has been three different things in a week, and the sequence is
the point.** It read `81` while a `tracepoint:` requirement was probed by
listing the event's *directory* under tracefs, which uid 1000 may traverse,
where the `id` file the attach actually reads is root-only: thirty sources
reported `available` and would have failed at attach. Finding 15 is that. When
the probe started answering from the access tracefs really grants it fell to
`62`, which was the first honest number and a worse tool. Finding 9's catalog
pass then put a `bpf:raw_tp` ahead of twenty-seven of those thirty — a raw
tracepoint attaches by name against the kernel's own BTF and needs no tracefs —
and finding 36 records what that moved. The seventh wave moved the last one:
`io.readahead` required `tracepoint:filemap:*`, and a glob is not a name, so no
probe covered it and the source reported `?` from both ends. It names two
tracepoints now. `87` is what this machine can serve through a path that
attaches, checked name by name against `/sys/kernel/btf/vmlinux`.

**The fourteenth wave changed what the number means.** A source used to be
called usable when any backend of its chain was viable, written or not, and on
the CI's runner `cpu.cycles` read usable through `pmu:sw`, which nobody wrote,
and every run of it exited 3. It is usable now only through a backend with a
collector (`docs/portability.md`): `power.throttle`, `gpu.kernel`,
`mem.bandwidth`, `net.xdp` and `gpu.counters` left usable or unverified here,
and `cpu.profile`, `cpu.oncpu_time`, `io.bytes` and six more now name the
written backend a request resolves to. None reads unverified any more.

It cuts the other way too, which cost a session to find: a binary that carries
capabilities is non-dumpable, so its own `/proc` entries belong to root and the
three `mem.heap.*` sources lost the `pagemap` they read. Finding 14 of
`docs/open-findings.md` has it; `main.rs` now restores dumpability at startup.

Per family, sources with a collector:

| Family | With a collector | Sources | Usable here |
|---|---|---|---|
| `mem` | 22 | 23 | 22 |
| `net` | 15 | 16 | 14 |
| `io` | 14 | 14 | 14 |
| `cpu` | 13 | 14 | 12 |
| `sched` | 10 | 10 | 10 |
| `gpu` | 10 | 10 | 9 |
| `proc` | 6 | 6 | 6 |
| `power` | 5 | 5 | 4 |
| `runtime` | 4 | 4 | 4 |

**`io`, `sched`, `gpu`, `proc`, `power` and `runtime` are complete** (`gpu`
since the nineteenth wave; `gpu.job` is out of reach here, a DRM scheduler
NVIDIA's driver does not use), `net` gained `net.request` for the vertical
case, and the rest are one short. The section below names what the last three
wait for.

---

## What works end to end

Each of these has a ground-truth test: a check against something independently
known, not against what the code computed.

| Source | Mechanism | How it is checked |
|---|---|---|
| `cpu.instructions`, `.cycles`, `.ipc`, and the rest of the counters | `perf_event_open`, counting | a hundred million known instructions, user-mode only, within 1 % and with a run-to-run spread under 1 % |
| `cpu.util`, `.pressure`, `.throttle` | procfs, cgroup | against `/proc/stat`'s own accounting |
| `mem.rss`, `.pss`, `.uss` | procfs | two backends cross-checked against each other |
| `mem.faults` | `perf_event_open`, sampling | against the kernel's own minor-fault counter for the same process; in user mode where the kernel allows no more, a block the program writes gets its faults and one only `read()` fills gets none |
| `mem.heap.alloc`, `.live`, `.retained` | `LD_PRELOAD` shim | allocation sizes the test chose |
| `io.bytes`, `.iops`, `.inflight`, `.merges`, `.pressure` | `/proc/diskstats`, PSI | a device resolved through `mountinfo`, with known I/O pushed through it |
| `net.bytes`, `.packets` | `/proc/net/dev` | 16 MiB pushed through loopback |
| `net.tcp.rtt` | netlink `sock_diag` | a loopback connection found in the dump, keyed by its cookie |
| `net.iface`, `.backlog` | procfs | a one-deep accept queue given 64 connections |
| `power.energy` | RAPL through perf, and sysfs | about 40 W package draw, scale read from the kernel |
| `power.freq`, `.cstate`, `.temp`, `.throttle` | sysfs, MSR | `/proc/stat`'s independent idle accounting |
| `gpu.freq`, `.power`, `.temp`, `.mem`, `.engine_busy` | NVML through `dlopen` | 256 MiB of VRAM allocated and read back to the byte |
| `proc.fd`, `.threads` | procfs | a launched process that reports its own counts; asserted equal, not close |
| `runtime.jit` | `/tmp/perf-<pid>.map` | a map the test wrote |
| `sched.wakeup` | eBPF raw tracepoint | the causal chain of a shell and its children, loaded and attached for real |
| `sched.switches` | `perf_event_open`, and a `sched_switch` program for a cgroup | against the kernel's own switch counter for the same thread; per cgroup, split voluntary and involuntary within each thread's own `nvcsw` and `nivcsw` |
| `sched.offcpu` | eBPF raw tracepoint | a thread that sleeps a known 300 ms, whose off-cpu duration and interruptible state must both match |
| `cpu.profile` | `perf_event_open`, sampling with `PERF_SAMPLE_CALLCHAIN` | a cpu-bound loop in the test binary, whose own symbols must appear in the table |
| `cpu.profile --stack fp`, `fp+leaf`, `dwarf`, `auto` | the kernel's frame-pointer walk; one `framehop` step over 256 copied bytes; a `framehop` walk over 8 KiB | one C program built four ways, whose time goes to one leaf in shares known by construction (20/40/30/10 %): each branch's whole path from `main` and its share, within four binomial deviations; `fp` loses the frameless leaf's caller in ≥ 95 % of its samples and `fp+leaf` puts it back, marked (`docs/unwind.md`) |
| `gpu.kernel` | CUPTI's activity API, from the injection library | five PTX workloads driven through `libcuda`, toolkit-free: kernel, copy and memset counts exact against each workload's own (20 000 launch-bound, 4 streams of 25 on 4 stream ids); each kernel's duration within `[span, span + max(10 us, 1 %)]` of a self-timing kernel's own `%globaltimer` span (+1.0 to +1.3 us on 1 ms), its times inside the host's launch window; with two 64 KiB buffers, kept + dropped = launched exactly, the run saying `LOSS: 8737 dropped`. Costs `extreme` launch-bound (+67 %), under 1 % on long kernels, against a negative control |
| `gpu.counters` | CUPTI's range profiler in kernel replay | two workloads that disagree about one counter: `dram__bytes_read.sum` reproducible on a 64 MiB copy and not dominated on a compute spin; instruction counts bit for bit on both, on the launches `sm__ctas_launched.sum` shows no other process reached. Its cost is not yet measured |
| `gpu.profile` | CUPTI's PC sampling, continuous | a kernel whose hot loop is known: line 20 holds 95.9 % of the samples; samples against the self-timing kernel's own cycles over the period, 1.0006 at `2^12` and 1.0014 at `2^14`; a kernel without line information placed by offset. Its cost is not yet measured |
| `gpu.*` through NVML, the device's state | NVML through `dlopen` | the clock lock, application clocks, persistence mode and clock event reasons stated by every NVML run and every GPU timing, persistence and the max shader clock agreeing with `nvidia-smi`; readings the driver had not refreshed counted (`driver_sample: repeat`) |
| `gpu.power --mode count --emit rate` | NVML's energy counter at each end of a window | the board's mean draw in watts, against `nvidia-smi` sampled from another process over the same window (21.09 W against 20.47 W) |
| `--container NAME` | the cgroup its runtime names | 388 blocks kept of the task inside, 387 by the kernel's count, 0 of the task beside |
| `sched.offcpu --emit stack` | eBPF `bpf_get_stackid` into a stack map | the verifier accepting the hand-assembled program, and a 300 ms sleep coming back with the twelve frames that led to it |
| `sched.runqlat` | eBPF, bucketed in the kernel | a known number of blocking round trips, found by four sources at once |
| `sched.migrations` | eBPF classic tracepoint | two hundred migrations driven on purpose with `sched_setaffinity` |
| `mem.pressure`, `.swap`, `.thp`, `.cgroup`, `.numa`, `.wss` | procfs, sysfs, cgroup2 | per-node residency against a known allocation; PSI against the kernel's own stall counters |
| `mem.cache_l1`, `.cache_llc`, `.cache_tlb` | `perf_event_open`, `PERF_TYPE_HW_CACHE` | a shuffled walk over 64 MiB against one over 4 KiB, and the descriptor packing checked field by field |
| `cpu.oncpu_time`, `cpu.util` per core | procfs, per-cpu perf | a spin loop pinned to one core with `taskset`, started from outside the tool |
| `cpu.oncpu_time` per thread | eBPF raw tracepoint on `sched_stat_runtime` | a shell blocked at both ends of the window: its rows sum to its own `schedstat` difference exactly (`assert_eq!`); a launched program's rows to its reaped cpu time within 47 to 68 us, what it ran before the gate |
| the nine views of `omn top` | a recording of each view's own workload | each view drawn on its workload's recording reads the manifest's numbers off the screen, within each figure's trust (`tests/view_truth.rs`): the flame's weights, the timebar's split, the wake ring, bytes per file and per socket, the heatmap's regimes, the lanes' order and twelve kernels, each request's cause |
| `net.capture` | `af_packet` with a `PACKET_MMAP` ring | a known number of frames of a known size over loopback |
| `net.hwts` | netlink `ethtool` | whose clock a NIC would stamp with, and where it is |

The two correlations. From `examples/allocation-cost.yml`, an interval match:

```
join pages-of-allocation  mem.heap.alloc -> mem.faults  evidence KEY
  2655 of 3518 `mem.faults` events attributed (75.5 %); 863 fell outside every open range
  states: 2555 opened, 2524 closed, 31 still open at the end, peak 1579 held at once
  no eviction: nothing was dropped for age or for space
```

And a wakeup match, which is the one that crosses a block:

```
join what-released-the-block  sched.wakeup -> sched.offcpu  evidence WAKEUP
  3 of 3 `sched.offcpu` events attributed (100.0 %); 0 followed no wakeup this run saw
  heaviest wakeup chains
    CHAIN                                     EVENTS  OUTCOME
    task:2017812 -> task:2017811                   1  ran
```

---

## Where each roadmap phase stands

| Phase | State |
|---|---|
| 0 — decisions | **done**. Language (Rust, no libbpf), clock, scope, a `kernel>=` per backend rather than a floor (5.15 served), `CAP_BPF`+`CAP_PERFMON`, raw tracepoints first; the three stale lines amended in the fifteenth wave. |
| 1 — event model and request schema | **done**. `Event`, `Span`-equivalent attributions, `Request`, `ResolvedRequest`, typed keys, one clock domain. |
| 2 — catalog, resolver, CLI | **done**. 100 entries as data, machine introspection, a resolver that records why each backend was passed over, a validator that refuses before attaching. |
| 3 — first collectors and transport | **done**. Ring buffers for perf and eBPF, loss accounting including throttling (per ring, since the fourteenth wave), a launch gate, unwinding by frame pointers, frame pointers and the leaf's caller, or DWARF (the fourteenth wave), and symbolization through the target's own maps, or debuginfod when asked. |
| 4 — the vertical case | **done, except `dm-delay` in the server's own decomposition.** `dm-delay` is injected since the sixteenth wave and held at the I/O layers (`tests/dm_delay.rs`: 482.0 ms above the device for 24 I/Os of 20 ms), not yet a row of the vertical server's tree. One request decomposed boundary by boundary on a real server, the lock's holder and the block device named, and since the tenth wave the tree's two network rows: a client that pauses 30 ms mid-request puts 30.14 to 30.27 ms in `net_rx`, and a slow reader puts 32.8 to 33.8 ms in `net_tx`, both injected from the client without root. The rows sum to the client's latency within +0.06 % to +0.36 % (+3.1 % for the slow reader, whose last buffer the client reads after the server is done). The network rows print `IDENTITY` where they are `KEY`; the engine change is filed. |
| 5 — generalizing the join engine | **done**, and a fifth kind since the eighth wave. The hour of joins ran to its end in the sixteenth wave (1500 rounds, nothing live, 0.7 MiB a round), `overlap` takes `enclosing:` for a drained row (and a stall row carries its last end since the seventeenth), and `iface` is joined by its namespace and its index there since the seventeenth wave. `interval`, `eq`, `scoped_eq` and `wakeup` each have a producer; `between` is the one three crossings asked for independently, and it is `TEMPORAL` by construction — a polled total's change credited to what moved with it, which the ladder must not call `KEY`. Beside the joins, a `decompose:` block with a signed residue that is never rounded away. Since the eighteenth wave a rule chooses its tier: `query` applies the same engine to a stored run's `raw` events (`join::apply_stored`), `kernel` is refused with the reason until a match kind has a program; since the twentieth wave `omn join -f <rules> <results>` applies every rule of a session to a stored run. |
| 6 — emit, aggregation, budget | **done but for one measurement**: the 2 M IOPS overhead of the in-kernel histogram. Run once before the seventeenth wave, it read 23.57 % (781 ns a request at 3.16 M IOPS); the wave gave the histogram its own program pair, whose cost is the coordinator's next `null_blk` run, and whether 1 % is the right criterion is open (`DECISIONS.md`). The eighteenth wave found why the program statistics read 0 runs (the block programs were closed at the stop, before the run read their account) and keeps them loaded until the close. Run after its merge: 11.54 %, 332 ns a request, of which the programs' own run time is most (298.5 ns a request with the statistics on, which add about 125); `tp_btf` is the next cut, and the criterion is still open. A kernel-built row's sum is its exact `total_ns` in every emit since the seventeenth wave. Since the sixteenth wave the budget's ladder steps from `trace` to `sample` on a collector that weighs its samples (`mem.faults` on `pmu:sw`), held to the task's own fault count across the move, and weighs the target in nanoseconds. Every emit exists, and every declared output format has an encoder — `folded`, `speedscope`, `pprof` and `chrome` included. A run measures its own cpu against the target's and is held to `--budget`, degrading or stopping and saying which. And since the tenth wave, **hot attach**: a request carries a `window` inside the run, attached and detached while the rest keep running, and two sources attached for one phase each of a program count that phase within +0.17 % and nothing of the other. Three edges are filed with the files that own them: the join engine credits events after a detach to a stale state, the results document does not yet say where a window was, and a per-task counter attached late to a program that already has threads counts one of them. |
| 7 — declarative configuration | **done**, every criterion: since the sixteenth wave's sweep every gap between two fresh matrix cells is explained by their resolved records. Sessions in YAML, JSON and TOML; a dumped request reads back unchanged; the resolved file is written beside the results. |
| 8 — self-benchmark | **done**; the second table ran before the seventeenth wave (68 minutes, its test green, 46 pairs flagged over their declared band, worst `io.fsync` through `bpf:fentry` at 142 %), and since the eighteenth wave the bands are corrected to it: 43 `cost:` lines moved, two stand with their reason, the table is recorded in `docs/bench.md` and `tests/bench_bands.rs` fails when a band and its row disagree. A table re-measured after the wave's fixes is the next run. Since the nineteenth wave the GPU tracers have a table of their own, per regime and against a negative control (`gpu.kernel` `extreme` from +67 % launch-bound, `gpu.api` `high`), held by `tests/bench_gpu.rs`. The harness has every task the phase lists, and it has measured: 94 (source, backend) pairs alone on this machine, every workload admitted by a gate that judges what a median resolves and refuses a busy machine (`docs/bench.md`). The resolver's seven choices between served backends were confirmed. What the table corrects is the cost bands: an in-kernel collector's cost is in the target's dilation, not the tool's cpu (`proc.syscall`: 0.42 % against +296 %), and the preload shim cost an allocation-bound program about 150 times -- two system calls a record -- until it was moved onto a shared ring, which brought a record to about 70 ns. Finding 47 files each. |
| 9 — breadth | nearly done; **115 of 118 sources, 133 of 226 backends** since the twenty-fourth wave added sixteen sources (`cpu.branches`, `mem.system`, `mem.oom`, `io.util`, `io.vfs`, `net.snmp`, `net.sockets`, `net.tcp.accept`, `sched.interrupts`, `sched.runqueue`, `sched.slices`, `power.wakeups`, `gpu.mem_freq`, `gpu.throttle`, `proc.calls`, `proc.tasks`) and new backends to existing ones (`cpu.oncpu_time`'s task-clock, `power.freq`'s APERF/MPERF, `mem.numa`'s `fs:sys`, `mem.swap`'s counting kprobe). The three without a collector wait on hardware this machine lacks; the vendor library (CUPTI) was the nineteenth wave's. |
| 10 — intelligence layer | **its first method is done by the engine**. TSA — where a thread's wall clock went — is what `examples/join-decompose-compute.yml` produces: three states, each row's kind and resolution, the program's life as the whole, and a residue inside its own resolution. Since the tenth wave the layer above it exists and calls the engine like any other user: the objective is required and refused when missing, `omn latency` names what dominates a run or its tail, `omn saturate` reads a queue by Little's law, and `omn diff` subtracts two profiles -- each checked against a workload whose limit the test chose. Since the eleventh wave **the escalation** closes it: a session's `escalate:` block attaches the layer the cheap pass names mid-run, detaches the rest and holds the budget; a program cpu-bound then lock-bound gets the lock layer 0.4 to 0.5 s after the switch (`docs/triage.md`). **Done.** Since the eighteenth wave `omn latency` follows a wait for a child into the child that ran during it and names the busiest other thread beside a block it ran in, asks `sched.mutex` only in a second pass, and takes `--symbols`. |
| 11 — a model reads it | **done** (the twelfth wave): `omn-mcp`, an MCP server over stdio in the workspace, exposing the catalog, the runs a directory holds and this machine's sweep, as the documents themselves regrouped per request with what each is missing stated first. It probes nothing (the `omn` binary answers every machine question), and runs a session only when its operator allowed it, bounded, budgeted, and never one that launches a program (`docs/mcp.md`). What it filed: `omn list --format json` carrying the sweep, so the server stops parsing the text table. Since the fourteenth wave the use case runs end to end: an agent benchmarks a launched program through approved sessions and says what dominates its time, every number it is handed held to the run's files (`docs/mcp.md`, "The walk-through"). Since the eighteenth wave one privilege model: the capabilities are on `omn` alone and `omn-mcp` collects through it, shown on a `CAP_BPF` run; `run_session` takes an optional budget, says what a budget is a share of (`budget_against`) and hands back the program's stdout. |
| 12 — programs nobody wrote for it | **started in the seventeenth wave**: a case is a directory (`cases/<name>/case.yml`, `docs/cases.md`), asked both ways by `tests/cases.rs`, `omn latency` and `omn-mcp`. Seven cases since the eighteenth wave brought `gcc-shell` back (`sqlite3-stripped`, `sqlite3-durability`, `cpython-interpreter`, `cpython-perf-map`, `http-server-stdlib`, `make-sqlite`, `gcc-shell`). The first suspect is the known cause, both ways, for every cause asked here, `make-sqlite`'s one-job cause in four runs of five; what still gets the cited numbers wrong (a job is a chain of processes, a thread that waits beside the main thread) is filed; since the twentieth wave the triage follows a thread blocked on a connection beside the block (`waiting`), carries the rest of a job (`followed.job`) and never names its own eBPF frames, shown on its own workloads and not yet on those cases. Four of six exit criteria met since the twenty-fourth wave followed `http-server-stdlib`'s slow client on the agent's way too (no case ends with a confident wrong suspect; and a seventh program is a directory, a stripped binary named from debuginfod both ways, and every case at paranoid 2: run after the merge, each cause answers as at -1 with the capabilities and skips both ways with the machine's reason without them, through the skip gate); `ROADMAP.md` says which are partly. |

---

## What remains, in the order it is worth doing

**This section is the one place the plan after the current wave is written.**
`BRIEF.md` is the wave being run; there is no second brief for the one after it,
because a plan kept in two documents drifted in both.

The eleventh wave -- the escalation, the engine's debts, the silent
degradations, the drains and the portability matrix -- is merged, and
`docs/briefs/wave-11-escalation.md` keeps its brief. **The twelfth wave is merged**, all five streams, and
`docs/briefs/wave-12-recorder.md` keeps its brief: the flight recorder (R),
phase 11's MCP surface (M), the kernel-side attributions and drains (D --
finding 48 closed), what the resolver promises (W -- P7 to P9), and what a
probe asks (V -- P1 to P6 and P10). What is left of the matrix's classes is
two known defects and a new one (P11, `cpu.profile --system` on 5.15), and a
matrix rerun on an idle machine for the cells swept before the fixes.

**Found at the eleventh wave's merge:** `cross_compute`'s lossy-profile
decomposition failed deterministically on `main`. The tool was right and the
test was not: since `cpu.profile` follows the processes the target starts, its
dropped-sample count is run-wide across every sampled task's ring, and the test
credited all of it to the one task it decomposes. The test now weighs the drops
against every sampled task's room (`docs/fixes/silent.md`). What it leaves: a
profile keyed by `task` cannot say *which* task lost which samples, though each
ring belongs to one -- losses per ring carried into `Collection`, a change to
`src/emit.rs` and the collector layer, after the flight recorder.

**The thirteenth wave is merged**, all five streams, and
`docs/briefs/wave-13-cost.md` keeps its brief:

- **R, what a run costs the target** (`docs/fixes/budget.md`): finding 47 is
  closed. `cost.json` and `--budget` weigh what a collector spends inside the
  target -- the shim's own sampled time, the kernel's `bpf_prog_info` run time
  when it is timing programs -- and say *unknown* where nothing accounts for
  it, never zero; the bands follow the bench's dilation column. Losses per ring
  reach the emit layer; the shim's stream is bounded at 2^19 records.
- **P, the targets perf can serve** (`docs/portability.md`): P11 closed (5.15's
  64 KiB `RLIMIT_MEMLOCK`); the counting sources, `cpu.profile` and
  `mem.faults` serve a cgroup; the loader refuses a catalog that promises what
  no mode produces; the CI answers its first run.
- **S, the scheduler and the processes per cgroup** (`docs/families/kernel.md`):
  every `sched` and `proc` program compares a cgroup and everything below it, a
  thread target compares the thread, `sched.switches` counts.
- **N, the older kernels** (`docs/families/devices.md`): the `io`, `net` and
  `mem` programs read the shapes 5.15 and 6.6 write, verified on both.
- **J, the engine's words** (`docs/families/join.md`): stacks demangled, `woke`
  and `overlap` matches, a decomposition part a join defines, and
  `omn-mcp --allow-launch`, so an agent can run the sessions its operator
  wrote and nothing else.

At the merge: 1994 tests green in one `--no-fail-fast` run, and the matrix
swept alone afterwards -- thirteen cells (5.15, 6.1, 6.6 and 7.2 under the
configurations the streams' fixes touched, `here-caps`, `here-nocaps`), every
judge passing and `KNOWN_DEFECTS` down to P10's `mem.cgroup`. A counted window
over a polled counter had lost its closing read on R's branch; found at the
merge, fixed before it (`fix/counted-window`).

**The fourteenth wave is merged**, all six streams, and
`docs/briefs/wave-14-product.md` keeps its brief -- the first product wave,
about the machine someone who is not the developer meets first:

- **P, the stock machine** (`docs/portability.md`): a source is usable only
  through a backend this build collects; the scheduler's counters are probed
  with the kernel included and refused where they would read zero; `pmu:ibs_op`
  is opened, not assumed; `mem.faults` samples in user mode at paranoid 2;
  `cpu.throttle` explains both cgroup shapes; the CI has two cells (`defaults`
  at paranoid 2, `stock` at 4) with their own expected skips, and the
  portability judge learned `stock` and refuses a source listed usable through
  an unwritten backend; `gpu.power` counts a window and its rate is watts; the
  validator checks a decomposition against the session's joins and a ranking's
  attribute against the entry.
- **R, the results say it** (`docs/fixes/results.md`): `Unit::Watts`,
  `KeyKind::Node` and `KeyNotCarried` first; every window closes at one instant
  before the first stop (72 blocks inside the window and in no row, before);
  `overlap` and `woke` print their amounts and the evidence level its sentence;
  `--container` is bound to the cgroup its runtime names; the shim's account
  leaves out a preempted span, counted and priced; a dump's format is refused
  before resolution; the CI's failures in R's files skip with the machine's
  reason.
- **K, the kernel programs** (`docs/families/kernel.md`): `fentry:`
  requirements carry prototypes (6.17's `tcp_v4_connect`); the read-ahead
  lookup is held exact in a run that lost nothing and repeated until one does;
  `sched.switches` gets a `sched_switch` program, second in the chain, serving a
  cgroup in `count` (the open decision, closed); `net.hwts` probed by its
  collector's own start; the runner's `/dev/root`, descriptor counts, exec
  naming and swap room answered; a sweep leg leaves out only the source the
  machine withholds.
- **J, an agent benchmarks a program** (`docs/families/join.md`,
  `docs/mcp.md`): the walk-through over `examples/mcp/approved/`, every number
  handed over held to the run's files and the spread computed from the answers
  equal to the files' to the bit; `overlap` with no key over machine-wide state
  (`join::MACHINE`), with `NoKey` and `SeveralSeries` refusals; `omn-mcp`'s
  `extreme` band from `Cost::worst_case_percent`.
- **D, it installs** (`docs/install.md`): `scripts/release.sh`, a reproducible
  archive with a glibc floor of 2.34 enforced, `make install` /
  `install-check` / `dist` / `dist-verify`, `release.yml` on a `v*` tag, and a
  README and `DEMO.md` for a stranger. An installed tree with the capabilities
  serves what `target/debug/omn` serves.
- **U, every way to unwind a stack** (`docs/unwind.md`): unwind modes as a
  chain (`catalog/unwind.yaml`) chosen by machine and target; `fp`, `fp+leaf` and
  `dwarf` (through `framehop`) served and held to a known call tree; `lbr` and
  `sframe` declared, probed and refused by name; user-only stacks at paranoid
  2, `--stack none` included; debuginfod names (the mechanism, not yet the
  flag); drops per ring; the frame-pointer note names the object the frameless
  leaves are in.

**At the merge**: 2068 passed, 3 failed, 8 ignored in one `--no-fail-fast`
run. The session-index test is closed by this commit, `scenario_cpu`'s
unwinder refusal by `1fbf331` (the list is `lbr`, `sframe` now that `dwarf`
is served), and `tests/portability.rs`'s
`every_cell_on_disk_holds_the_invariants` is **still red**: 267 violations in
the 13 cells under `target/matrix/cells/`, all "usable through `<backend>`,
which has no collector in this build" -- P's new judge reading cells swept on
2026-09-29, before P's probe fix. The current binary answers otherwise
(`cpu.profile` through `pmu:sw`, `cpu.oncpu_time` and `io.bytes` through
`fs:proc`). One `make check` went red once on
`page_faults::tests::faults_are_reported_at_the_address_that_faulted` (in the
tally, `docs/scenarios/cross-machine.md` row 2) and passed alone. `make
check` was not rerun after `1fbf331`.

**The fifteenth wave is merged**, all six streams (`12a48cd` to `1066646`,
then five merge fixes and the coordinator's swaps, `233d05b` to `fbe6f19`).
It had no brief of its own in `BRIEF.md`: it ran the list the fourteenth wave
left here, split by the files each item lands in. Each stream's handoff is
`docs/fixes/w15-<stream>.md`:

- **docs** (`w15-docs.md`): the three Phase 0 lines whose reasons no longer
  held amended (no libbpf, no single kernel floor, user mode at paranoid 2 as a
  stated subset); no `Span` struct and no reorder window, each decided with its
  cost; `docs/keys.md`, every key with the boundary it crosses;
  `docs/open-requests.md` refreshed at `d1f0182` (of the snapshot's open rows,
  67 landed, 4 superseded, 17 partly, 17 open; of later filings, 12 landed and
  20 open).
- **engine** (`w15-engine.md`): `join::decompose_within`, so a `paired` part
  pairs over the joins' windows (the coordinator swapped both call sites and
  removed `check_paired_parts`); one canonical integer, `Int(n) == Uint(n)`;
  no row without its evidence level; a load test of 1.5 M events with the
  open-state age index and unmatched states dropped (179 MiB of peak to 97);
  evictions end to end; the skip lines that put the machine's reason first.
- **resolver** (`w15-resolver.md`): `--symbols local|debuginfod` with the
  choice in `resolved.json`; `catalog/unwind.yaml`; `Backend.keys` and
  `KeyNotCarried`'s resolver half, naming what keeps the request from the
  backend that attaches the key; `StackRefused`; `NoBackendDeclares`; the
  no-viable-backend refusal leading with the collected backend passed over;
  phase 2's complete `ResolvedRequest` and phase 7's replay, both held to
  ground truth.
- **runner** (`w15-runner.md`): the aligned allocators hooked (a silent loss:
  `dd bs=64M` showed 4 KiB), a failed `realloc` no longer a free, the shim
  found under `lib/omn/`; `cost.json` whole (before, in and after the window,
  held to `wait4`); watts, not `J/s`; a budget's move counted across its seam
  and the overlap announced; the bench table's band column; `pub mod net`,
  `ProgramStats::for_run`, and "here" naming a build's gap.
- **probes** (`w15-probes.md`): `container: no` gone wherever a cgroup is
  served, and refused by the loader; a (target, mode) pair no backend declares
  is exit 2; a binary whose file capabilities the kernel withheld is told why;
  `cpu.cycles` on `pmu:sw`; the resolved record carries the privileges and the
  virtualization, a cell the binary that swept it; the phase 2 and phase 7
  audits over the cells.
- **kernel** (`w15-kernel.md`): `net.hwts` modes per backend, so the listing
  and the run agree; the bpftool cross-check that skipped everywhere now runs;
  zero loss under a stated moderate wakeup rate and an overflow by
  construction; `io.latency --emit hist` built in the kernel when it is the
  whole request; ten notes that carried their source's indentation.

**At the merge**: one `make check`, red on a probes test the kernel stream's
`net.hwts` modes made stale (fixed, `eb501ef`); then one `--no-fail-fast` run,
**2123 passed, 4 failed, 10 ignored** (the four are in "In one paragraph"
above; two fixed in `fbe6f19`), clippy `-D warnings` and fmt clean after the
fixes, `cargo test --test cli` green (26). No second full run. Eight exit
criteria of phases 1 to 7 moved to `[x]` in `ROADMAP.md`, each citing the
merged test that holds it: the `Event` round trip, the key note, two
listings that differ, a complete `ResolvedRequest`, the loss rate at zero
under moderate load, every line's evidence level, evictions in the output,
and the CLI-to-file replay.

**The sixteenth wave is merged**, all six streams (`1496d5a` docs, `2b35f76`
engine, `87d4d65` runner, `d4206ed` cpu, `cb53a61` io, `5a083f1` mem, each
`--no-ff`), then the cross-stream requests the streams filed (`8d80fd1`), the
tests they made stale and the three new sessions indexed (`49e8fd2`), and the
gap judge's two defects (`3e78c7c`). Like the fifteenth, it had no brief in
`BRIEF.md`: it ran the list the fifteenth wave left here, split by files. Each
stream's handoff is `docs/fixes/w16-<stream>.md`. The only conflicts were
appends at the end of `docs/open-findings.md`, both sides kept.

- **cpu** (`w16-cpu.md`): a forced `--stack` mode the machine lacks is exit 1,
  the machine's gap; the CI's reds in the stream's files each held to what the
  kernel or the workload says (the dwarf copy to the target's pagemap, the
  throttle to the kernel's ceiling read first, the profile's threads burning
  in user mode, `oncpu` to `schedstat`); a user-only profile no longer reads
  `/proc/kallsyms` (98 bytes read against 19 MB), and the factory takes the
  resolver's stack choice; `Backend.unit`, so `cpu.cycles` through the cpu
  clock reports `ns` (the result's half landed at the merge); seven
  continuations that had lost their `\`.
- **io** (`w16-io.md`): both CI reds held to the kernel's own counts (merges
  from both sides of the window, the queue depth to the kernel's `inflight`
  read at 1 ms); a join over a histogram the kernel built refused by the
  validator, exit 2, only where `io.latency` builds it; `keys:` on `io.bytes`
  and `io.iops`, the first shipped `KeyNotCarried` (exit 3); the edge-crossers
  bounded by a measured clock skew; **`dm-delay` run for real**
  (`tests/dm_delay.rs`); the eBPF skip lines leading with the missing pieces;
  `.github/expected-skips*.txt` and the bpftool link in the workflow.
- **mem** (`w16-mem.md`): the paranoid-2 user-mode retry of `mem.faults`,
  which had never run (D14), and two tests that now run against the
  program's own faults on `defaults`; the reclaim test under `memory.high`;
  the zero-page swap test's failure message carrying the vmstat deltas;
  `scenario_net`'s trace leg deciding the machine from sysfs's clocks;
  `permission_advice` naming why a grant did not reach the process;
  `net_probe` gone; the shim checks matching the tool's words; the interface
  index filled by `net.bytes` and `net.iface`; `huge_folios` (D15) and the
  cache tests as separations; flaky #3 and #21 closed.
- **runner** (`w16-runner.md`): the mode rule and a ladder across modes
  (`mode_move`, `Ladder`), which since the merge really moves `mem.faults`
  from `trace` to `sample` on weighed samples; the nanosecond readers of a
  process and a thread, which the `Meter` weighs since the merge; the
  `cost:` line naming the cpu before the window; a budget's move no longer a
  pin (`resolve_onto`); `FilterFieldNotCarried` naming its elsewhere for what
  keeps the request from it; the bench's whole-table check.
- **engine** (`w16-engine.md`): the resident-size decomposition's CI red
  (the interpreter's startup, now bounded by a `READY` line); `iface` compared
  by its index; `overlap`'s `enclosing`, all of pgsteal credited where 1 %
  was; the engine's memory bounded by its map and `top`, collections walked
  apart (`apply_merging`, called by `assemble` since the merge); the hour
  test as its doc says; three sessions.
- **docs** (`w16-docs.md`): watts, not `J/s`, in three documents, re-run;
  `docs/install.md`'s kernel section per backend, with the counts' commit and
  date; `docs/open-requests.md` re-read at `37f17c8` (19 later filings moved
  to landed); `README.md` and `DEMO.md` re-run end to end.

**At the merge**: one `make check`, red; then one `--no-fail-fast` run,
**2153 passed, 4 failed, 10 ignored** (the four are in "In one paragraph"
above: two fixed in `49e8fd2`, the judge by the sweep and `3e78c7c`, and a
flake that passed alone).
Clippy `-D warnings` and fmt clean after the fixes, `cargo test --test cli`
green (26), `cargo test --test portability` green (25) after `3e78c7c`. No
second full run. `/home` was at 99 % during the wave (`No space left on
device` in every stream's builds); the merged worktrees' `target/` directories
were deleted at the merge, and it is at 94 % now.

**And the quiet-machine tasks, run alone after the merge:**

- **The matrix**: all 22 cells swept again with one binary
  (`cd394a8a8550c199`), every guest cell complete with 0 broken, and
  `6.1.188-caps` swept again (it had only a qemu `vhost` error before). Every
  cell records `privileges`, `virtualization` and `cell.build`. Usable, with
  the capabilities through to without:

  | Kernel | caps | root | notracefs | nobtf | nocaps |
  |---|---|---|---|---|---|
  | 5.15.221 | 71 | 75 | 75 | 45 | 41 |
  | 6.1.188 | 74 | 78 | 78 | 45 | 41 |
  | 6.6.157 | 78 | 79 | 79 | 45 | 41 |
  | 7.2.7 | 80 | 81 | 81 | 45 | 41 |

  and here 92 with the capabilities, 56 without (`here-nocaps` was swept from
  a relinked binary, then `make` put the capabilities back). In `here-caps`,
  `net.hwts --system` now runs in `poll` through `netlink:ethtool` and exits
  0; the five stale violations of `here-nocaps` are gone; the invariants
  judge passes. The gap judge first left 58 gaps unexplained, all its own
  defects (`3e78c7c`: a uprobe refusal with two causes, uprobe-multi below 6.6
  and `CAP_SYS_ADMIN`, now `Cause::KernelBelowOr`; and `net.hwts`'s
  unwritten `so_timestamping` probe read as the build's difference). Among
  the 22 fresh cells every gap is explained, none is the build's, none waits
  on a missing field. The five `7.2.6-*` cells on disk predate the host's
  7.2.7 and record none of the fields: they alone account for the judge's
  1316 "not explained yet" (`rm -r target/matrix/cells/7.2.6-*` clears them).
  `sched.switches` is usable on 5.15 through `pmu:sw`, which answers the docs
  stream's question about `docs/portability.md:184`. Logs:
  `target/matrix-here.log`, `target/matrix-all.log`.
- **The join hour**: passed. 1500 rounds of 2 001 347 events (790 600
  states) in 3600 s, nothing live at the end, every round's peak 0.7 MiB
  beyond the stream's 427.0 MiB, each round applied twice in 1.8 to 2.0 s
  (`target/join-hour.log`).
- **The bench's second table**: **not run.** `scripts/bench.sh` measures
  `target/release/omn` and stops when it carries no capabilities, and the
  sudoers line covers `target/debug/omn` only. `make release` is built; the
  `setcap` and the bench are in "Needs privilege" below.

ROADMAP boxes ticked at this merge, each citing its test: phase 5's hour
(`join_engine::an_hour_of_joins_leaks_no_state`), phase 6's announced
degradation (`budget::a_traced_series_over_its_ceiling_counts_what_the_task_did_and_moves_only_onto_weighed_samples`)
and phase 7's gap between two machines
(`portability::every_gap_between_two_cells_is_explained_by_their_resolved_records`).
Four open decisions settled (`DECISIONS.md`, "Settled in the sixteenth wave"),
three new ones open.

**Before the seventeenth wave**, the coordinator ran three of the privileged
tasks the sixteenth left (2026-10-03): **the bench's second table**
(`scripts/bench.sh`, 68 minutes, its test green; 46 (source, backend) pairs
flagged over their declared band, worst `io.fsync` through `bpf:fentry` at
142 %, then `net.tcp.connect` 39 %, `gpu.freq` through `nvml` 14.5 %,
`power.cstate` 7.8 %, `cpu.profile` through `pmu:sw` 4.9 %; documents under
`target/bench/backends-1791030430/`); **`netem` on lo**, under which
`join_engine::a_queued_packet_on_lo_joins_lo_s_link_counters_by_its_index`
passed for the first time; and **`null_blk`**, under which the in-kernel
histogram's test read 23.57 % at 3.16 M IOPS against phase 6's 1 % -- the
subject of the wave's io stream. **The push is blocked**: the agent shell has
no ssh key and the `gh` token lacks the `workflow` scope, so the CI has not
been read since `37f17c8`, and everything since is unpushed.

**The seventeenth wave is merged**, all seven streams, and
`docs/briefs/wave-17-programs.md` keeps its brief: **ROADMAP phase 12**,
programs nobody wrote for it, beside the sixteenth wave's engine and core
filings and the in-kernel histogram's cost. Two merges: `500c4fd` symbols,
`c7a3237` cases, `5f85e5b` engine, `5021a29` core, then `d3a829c` (three
mechanical cross-stream requests); and `f7b38ba` python, `55b6019` build,
`6ba8e74` io, then `ae889f7` (the five new cases in `docs/cases.md`'s table).
No conflict in either. Each stream's handoff is `docs/fixes/w17-<stream>.md`:

- **symbols** (`w17-symbols.md`): **a perf map is a symbol source**, read
  under `--symbols local`. An address in an anonymous executable mapping that
  no file names takes the name of the `/tmp/perf-<pid>.map` line covering it,
  read as the map grows, again on each profile tick once a frame went
  unnamed, and once more at `stop` (`Unwinder::finish()`, wired into
  `cpu.profile` and `mem.ibs`). Every frame says where its name came from
  (`Frame::named_by`: `elf`, `debuginfod`, `kallsyms`, `perf-map`); what it
  could not name is a note with its reason; `runtime.jit` shares the one
  parser. **Shown**: a generated page named from the map the test wrote, in
  process and through `omn cpu.profile`, a map written at exit included, and
  `python3 -X perf` naming `py::omn_hot_loop` on more than half the samples.
  **Reasoned**: the off-cpu sources get the on-miss read but not the
  renaming; `node` and a JVM not installed here.
- **cases** (`w17-cases.md`): **a case is data**. `cases/<name>/case.yml`
  (the program, how it was obtained, `needs`, `fetch` into
  `$XDG_CACHE_HOME`, per cause the injection, the references read by named
  extractors, `holds`, `expect`, `open_findings`); `tests/cases.rs` discovers
  every directory and asks every cause **both ways** -- `omn latency` as a
  person would, and the same question's dumped session approved into
  `omn-mcp --allow-launch` and read as `docs/mcp.md` tells a model -- against
  ten named checks. A listed open finding prints `FINDING` while it fails and
  fails the test once it passes. Case 1, `sqlite3-stripped`: a plan that
  scans (40 M fullscan steps, `EXPLAIN QUERY PLAN` and `.stats`) and a lock
  held by another process for a known time. **Shown**: the known cause first
  both ways; the on-cpu row 1 % over `.timer`'s cpu, the blocked row 29 ms
  over the holder's clock; the agent's way names `sqlite3VdbeExec` from
  debuginfod and says so. **Five findings filed** against the triage and
  `omn-mcp` (no `--symbols` on the triage, the names note dropped, an off-cpu
  stack not asked through debuginfod, `Limit::Sleep` calling a polled lock
  not a lock, a waiting program over every budget).
- **engine** (`w17-engine.md`): all three sixteenth-wave filings. A stall row
  carries its **last end**, so a drained row's enclosing window is first
  episode to last end (the 100 ms reclaim session credits 99.5 to 100 % of
  the cgroup's pgsteal with it, measured three times; its session line is
  filed). **`proc` is carried** by `proc.fd`, `proc.threads` and `mem.{rss,pss,uss}`
  after `task`. **The network namespace is in an interface's identity**
  (`iface#<netns>:<index>`), read in the kernel by `net.qdisc`; another
  namespace's interface is another key, labelled `@netns:<inode>`.
  **Shown**: 2000 packets on another namespace's lo, credited 0 and
  uncredited 2000 (by hand, and as a test applied and reverted, filed for
  `tests/join_engine.rs`), where the sixteenth wave measured all 2000 credited
  here. **Reasoned**: `possible_net_t`'s offset taken as 0 (a typedef
  `member_offset` does not follow, filed); no kernel without `CONFIG_NET_NS`
  run.
- **core** (`w17-core.md`): **a kernel-built row's sum is its exact
  `total_ns`** in `hist`, `summary`, `rate`, `topk` and `stack`, and `max`
  never below the mean; `cross_memory`'s skip no longer mistakes a served
  run's caveat, or a crash, for the machine withholding a source;
  `docs/portability.md`'s `sched.switches` row; **`LICENSES/GPL-2.0-only.txt`**
  installed and packed (`make dist-verify`: nine paths). **One order for
  "unbuilt" and "missing" is written and not landed**: `1aa114f` turns eleven
  tests outside the stream's files red where a pinned backend also lacks a
  requirement, so it is reverted and re-filed with each test's change.
- **python** (`w17-python.md`): cases 3 and 4. `cpython-interpreter` and
  `cpython-perf-map` (the same program, input and causes, differing by `-X
  perf`): a function dominant by construction of the input (cProfile 99.95 %)
  and a read of a named pipe written 1.5 s after the target appears;
  `http-server-stdlib` under the case's own client, the load and the clock: a
  30 000-entry listing, a slow client, and a cold 256 MiB file that skips
  here with its numbers (the NVMe is faster than the server's copy).
  **Shown**: the known cause first both ways for both CPython cases, every
  cited number within its slack; `py::score` on 1 043 of 1 056 stacks. **Seven
  findings filed**: the hottest leaf is never the Python function, an
  interpreter C function named as the suspect, a block's stack cut at four
  frames, a server's request is not its main thread's life, `sched.mutex`
  doubling a threaded interpreter's time (2 124 ms to 4 059 ms), "dynamic
  symbols only" not said, and a run the budget stopped saying the budget held.
- **build** (`w17-build.md`): case 2, `sqlite3-durability` (a rollback
  journal with `synchronous=FULL`, four `fdatasync` a commit, against WAL
  commits that wait for nothing), and case 6, `make-sqlite` (the critical path
  one job; a job's cpu shared with two competitors), referenced by a
  `schedstat` watcher (`cases/shared/watch.py`). Case 5, `gcc-shell`, is
  **held back** under `cases/held/` because its agent's answer is a coin
  between two tables of equal weight. **Shown**: case 2 right both ways; cases
  5 and 6 are the confident wrong suspect phase 12 was written against --
  the driver's and make's `wait4`. **Four findings filed**: a program is its
  main thread (follow a wait for a child), a budget weighs the launched
  process alone (and a stop leaves its children running), the mutex shim
  doubling a real program's cpu (and its clock read on calls that do not
  report), and a `blocked` row called exact 2 to 3.5 % short of the kernel's.
  The CI's workload step installs `sqlite3`, `strace` and `unzip`.
- **io** (`w17-io.md`, `docs/scenarios/io.md` defect 19): **the in-kernel
  block histogram has its own program pair** -- the open writes only the time
  into an entry that outlives its request, the close reads only the device and
  adds to a per-cpu row: 2 calls on the open where there were 6, 5 on the
  close where there were 11, no delete, no atomic. The cost test now asserts
  the histogram counted fio's reads before it trusts the percentage, and
  prints the per-request cost and the programs' own run time. **Not
  measured**: the run is the coordinator's, alone.

**At the merges**: the first `make check` green on its first run, **2189
passed, 0 failed, 10 ignored** over 51 binaries (`d3a829c`), with
capabilities and no `SKIPPED` line. The second red once (`cases`: an open
finding that a quiet run lets pass), then one `--no-fail-fast` run, 2190
passed and one in the flaky tally that passed alone; after the docs commit
`cargo test --test cli` 26 passed and a second `make check` **green, 2191
passed, 0 failed, 10 ignored** (`ae889f7`). No merge regression; no code fix
at either merge. The four first-merge worktrees' `target/` were removed
(`/home` at 77 %, 48 G free). `omn list` unchanged: 92 usable here, 96
sources with a collector, 108 of 205 backends. ROADMAP phase 12's last
criterion ticked, three marked partly, two not met; phase 6 and 8 notes for
the runs before the wave. Five decisions settled before the wave and
twenty-one by the streams (`DECISIONS.md`, "Settled in the seventeenth wave";
one of them, the order of "unbuilt" and "missing", decided and not yet in
the code), two new ones open.

**Before the eighteenth wave** (2026-10-03), the coordinator read the CI
at `b37a925` (run 37155975223): both cells red, on the skip gate (bpftool
missing from the runner's linux-tools, the memory controller not handed down),
the swap tests killed by their own limit, and the polled queue that never sees
a burst (`docs/open-findings.md`, "The CI at `b37a925`, read back"). The `gh`
token now carries the `workflow` scope, and `main` was pushed.

**The eighteenth wave is merged**, all twelve streams, and
`docs/briefs/wave-18-findings.md` keeps its brief: no new phase, but what the
seventeenth wave's programs showed -- the triage answering from the wrong task,
a profile costing 280 % of its target, an in-kernel histogram at 12 %, two
binaries with two privilege models -- and what the CI found. Two merges, each
stream `--no-ff`. Phase A, ten streams: `f95fef0` green, `3b55807` triage,
`2d87d82` names, `9c9155c` mcp, `be7d7f4` launcher, `1d434c5` profile,
`2ac4e3d` sched, `b67cfac` io, `e242224` engine, `988f008` bench. One conflict,
`catalog/io.yaml`'s `io.inflight` (io's comment on the two kernel accounts and
bench's measured `cost: low`, both kept). Then `983ea32` (names' glue patch:
`--symbols` set on every request whose collector reads it, `cpu.profile` and
since sched's merge `sched.offcpu`; the refusal lists `stack::symbol_readers()`;
the man page's Verdicts sentence), and `53df56f`, `cc3adc1` (the cases' open
findings as the merged wave left them). Phase B: `4bcddda` resolver, `e54c622`
cases (`cases/held/gcc-shell` moved to `cases/gcc-shell`), then `f51189d` (the
renamed tests cited by their new names, and the forced-backends header's two
exits). Each stream's handoff is `docs/fixes/w18-<stream>.md`:

- **green** (`w18-green.md`): the CI's bpftool is upstream's static build
  (`libbpf/bpftool` v7.7.0, pinned by version and sha256), Ubuntu's
  `linux-tools` its fallback, and the step fails when none dumps
  `/sys/kernel/btf/vmlinux`; the `defaults` cell is two delegating levels deep,
  because the skip was two `tests/cross_machine.rs` tests making one cgroup
  name; the swap tests push with `memory.high` (the runner **has** swap, and
  `memory.max` OOM-killed a child whose pages waited on a cloud disk); the
  working-set test walks 128 MiB, because a `clear_refs` flushes no TLB and
  the runner's cpu kept half of 4 096 small pages; `wss.rs` reads again after
  an ESRCH from a process an `execve` replaced. **Shown**: the step's both
  paths with a shimmed `sudo`, the skip gate over the run's two logs with
  those two skips removed, the TLB table, the ESRCH. **Only a CI run shows**
  the cells green; `encoders`' empty chrome trace on `stock` is *suspected*
  to be the same ESRCH in `footprint.rs`, filed, open.
- **triage** (`w18-triage.md`): **which task** -- a sleep or futex row is
  followed into the child that ran during it (in the wait's place, when the
  stack names `wait4` and its kin or the child was busy for a quarter of the
  block), into the busiest other thread beside it, or into a task no source
  placed (said unplaced); up to eight deep, `verdict.task` and the followed
  tables in the JSON and the text. `sched.mutex` is asked only in a second
  pass when a futex sleep is the largest row on the verdict's way; a source
  over 20 % of the target's cpu is said; `Limit::Sleep` names a polled lock
  and a wait for a child. **Shown**: `tests/triage.rs`'s child and thread
  workloads answered by the task that worked, its on-cpu row held to that
  task's `schedstat`.
- **names** (`w18-names.md`): `--symbols` on `omn latency` and
  `omn saturate`, set only where a collector reads it; a sample charged to its
  innermost perf-map frame, else its leaf (`diff::charged_to`, also `omn
  diff`'s); a flat interpreter profile said as one and naming no suspect; a
  block's stack printed to the program's own frame, 4 to 16 frames; the names
  note under each request's label; `docs/mcp.md` and `man/omn.1`. **Shown**:
  `py::score` on 1146 of 1162 samples under `-X perf`, the flat sentence
  without it, `py::wait_for_input` reached in the reader's block.
- **mcp** (`w18-mcp.md`): **one privilege model** -- the capabilities on
  `omn` alone, `omn-mcp` collecting through it, and `make install-check`'s
  eighth check comparing the two sweeps; `run_session` takes an optional
  budget, says per request what a budget is a share of (`budget_against`, a
  floor where the in-target part is unknown), and hands back the program's
  stdout, found apart from `omn`'s document byte for byte. **Shown** at
  paranoid -1: the server's sweep is `omn`'s (92), and an `io.fsync` run
  through it lands on `bpf:fentry` with nothing granted to `omn-mcp`. The
  `CAP_PERFMON` half at paranoid 2 is not shown. *Shown after the merge*:
  the protocol test passes at paranoid 2, and every case's two ways run with
  the same capabilities ("Needs privilege").
- **launcher** (`w18-launcher.md`): the budget's denominator is the program's
  process tree, read from `/proc/<pid>/task/*/children` and kept at its high
  water (a waiting shell read 1.84 ms, now 994.9 ms of its child's 1 s); a stop
  freezes and kills the tree; a run the budget stopped says so on its own line,
  and `cost.json` carries `stopped_by_budget`; a collector declares in the
  registry that it weighs its samples (the declaration is one line in
  `src/collect/mem/mod.rs`, outside the row, accepted at the merge). The
  runner's ring-loss test fails rather than skips, naming the gap.
- **profile** (`w18-profile.md`): the 266 % found. Not the mode (the triage
  forces `fp`), but a **one-time debuginfod symbol load** (about 25 ms of a
  debug build) weighed by the budget's first check against the 13 to 21 ms
  the target had burnt, beside a per-tick re-read of every followed process's
  maps, now done only for the processes a drain holds (`MapReads`: user-only
  13.2 to 58.2 % before, 7.1 to 28.7 % after). The user-only degradation says
  what it costs; `mem.faults`' sampled rung held on the paranoid-2 path;
  `collector_cpu_ns` held to `wait4` (1169.9 ms said, 1178.1 counted).
  **Reproduced by a patched build**, not at paranoid 2. *After the merge*,
  at paranoid 2 with no capabilities: 6 to 7.5 % without debuginfod, 11.5 %
  with a warm cache.
- **sched** (`w18-sched.md`): the target's **descendants followed in the
  kernel** (`sched_process_fork`/`free` and a map) by `sched.runqlat`,
  `sched.offcpu`, `sched.wakeup`, `sched.switches` (`bpf:raw_tp`) and
  `sched.migrations`; **a wait is the kernel's own account** (`rq->clock -
  last_queued`, plus `run_delay`'s growth): `sched.runqlat` 459.8 ms against
  the kernel's 459.2, children exact to the nanosecond in nine of nine; a
  block starts at `__schedule`'s entry (`sched_entry_tp`, 6.16);
  `sched.offcpu` named through debuginfod (against a local server and
  `debuginfod.archlinux.org`), "dynamic symbols only" said; `scenario_sched`'s
  probe name per run.
- **io** (`w18-io.md`, `docs/scenarios/io.md` defects 20 to 22): the
  statistics read 0 because the block programs were closed at the stop,
  before the run read their account -- they are now detached and kept;
  `io.inflight` reads the larger of `/proc/diskstats`' column and
  `/sys/block/<dev>/inflight`, carrying both (a workaround: why 6.17's disk
  line read 0 is open); a known-latency `null_blk` test, written; run after
  the merge, it fails at the granted 6 us load. The 348 ns is reasoned to be
  mostly the programs, and measured so after the merge (about 173 to 298 ns
  of a request is the programs' own run time; "Needs privilege").
- **engine** (`w18-engine.md`): no clock read in the shim on a call that does
  not report; `Btf::member_offset` follows a typedef (35 callers' names
  audited on the running kernel); the reclaim session's `enclosing:` (42 314
  of 42 314 pages credited); another namespace's lo credited 0 of 2000;
  **a rule chooses its tier** (`query` over a stored run, `kernel` refused);
  `join::sources_read`, whose use in `assemble` is a diff filed for
  `src/main.rs`.
- **bench** (`w18-bench.md`): 43 `cost:` lines corrected to the table, two
  standing with their reason (a fixed `bpf:fentry` attach), an `equivalent`
  backend never declaring less than what it stands in for (seven more lines
  raised); the table recorded in `docs/bench.md` and held by
  `tests/bench_bands.rs`.
- **resolver** (`w18-resolver.md`): `1aa114f` with the eleven tests
  (`e04ef9a`): a backend both unbuilt and missing a requirement here is the
  machine's on every path; the key hint ranks a runnable unwritten carrier
  above a missing one; the pinned `net.hwts` twin. **Shown** with and without
  the capabilities.
- **cases** (`w18-cases.md`): both readers follow the task the triage
  follows -- the person's down `followed`, the agent's by the same quarter
  rule over each task's own events, perf-map frames first, a flat profile
  naming nothing; two ways run with different privileges are not compared;
  `says:`, `held:`, `sometimes:` and a finding's `cause:`; `gcc-shell` back.
  **Shown**, seven runs: every cause's first suspect is the known cause both
  ways, `make-sqlite`'s one-job cause in four runs of five; `gcc-shell` both
  ways in five of five.

**At the merges**: phase A's `make check` ran three times. The last was green
on `cases` and red only on
`runner::a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed`;
the `--no-fail-fast` after it, **2271 passed, 4 failed, 11 ignored**: that
test (red on the base too, failing alone again: a gap filed for profile), one
in the flaky tally (`scenario_sched::a_forced_switch_program_...`, green
alone), and `scenario_net::an_interface_that_disappears_...` counted twice
(green alone, new to the tally). Phase B's `make check` stopped in the library
tests on two timing tests (one in the tally, one profile's new
`page_faults` test, 4064 faults weighed against 4099); the `--no-fail-fast`
after it had the library's 1482 green and every binary but `runner` green.
After the docs commit, `cargo test --test cli` 26 passed. Clippy and fmt were
clean on every stream's branch; on the merged tree they did not run, because
the red test step ends `make check` first. The ten phase A worktrees' and the
two phase B worktrees' `target/` were removed (`/home` from 13 G free to
52 G). `omn list` unchanged: 92 usable here, 96 sources with a collector, 108
of 205 backends. ROADMAP: phase 8's first two criteria and phase 12's
debuginfod criterion ticked, phase 12's "out of sight" criterion partly, phase
5's tier criterion noted. Three of the four lines decided before the wave
settled (the fourth, phase 6's criterion, waits on its run), with the streams'
lines and "one order for unbuilt and missing", now in the code
(`DECISIONS.md`, "Settled in the eighteenth wave"); one new line open.

**The nineteenth wave is merged**, all fourteen streams, and
`docs/briefs/wave-19-gpu-view.md` keeps its brief: two halves on different
files -- the GPU's inside, through the vendor's own library, and `omn top`,
a terminal view whose nine views are experiments the user judges on screen.
Two merges, each stream `--no-ff`. Phase A, three streams: `6c62e14` cupti,
`95170e8` device (`docs/families/gpu.md`, both appended sections kept),
`1f88f1f` core (the root `Cargo.toml`'s members, resolved to `["shim", "mcp",
"cupti", "view"]`), then `91a0cef` (clippy on the device stream's NVML code
on the merged tree -- a manual checked division and two `u32` casts, the one
real merge regression -- with `gpu.kernel`'s facts carrying the clock state,
`make check` and the CI's clippy over `--workspace`, and `docs/install.md`
listing `bin/libomn_cupti.so`). Phase B, eleven streams: `b1e8f6f` profiler,
`5f24461` bench (`CATALOG.md` and `docs/families/gpu.md`, both sides kept;
its message still carries git's `# Conflicts:` lines), `dbfc4e6` system,
`1833b2f` flame, `78d0cba` ledger, `49fdaa2` request, `3719d63` timebar,
`3440d02` heatmap, `1a9d2b0` lanes, `417263f` wake, `3efe46f` flows, then
`c6bd2b8` (`scripts/bench.sh gpu`; the lanes view declaring `sched.wakeup`
with its README row; one cell between the run's length and the cost corner in
the header). Each stream's handoff is `docs/fixes/w19-<stream>.md`:

- **cupti** (`w19-cupti.md`): the injection library `cupti/`
  (`libomn_cupti.so`), loaded by the driver from `CUDA_INJECTION64_PATH`,
  CUPTI opened at runtime, records batched over a sequenced-packet socket
  with bounded sends; `gpu.kernel` through `vendor:cupti` for a launch, a
  `pid` refused with the entry's own reason; GPU timestamps converted by
  measured brackets (`src/clock.rs`). **Shown** (`tests/gpu_kernel.rs`, 11):
  kernel counts exact on five PTX workloads; zero dropped at 50 000 launches
  with the default buffers, and 8 737 dropped and said with two small ones,
  kept + dropped = launched; each self-timed kernel within
  `[span, span + max(10 us, 1 %)]` (+1.0 to +1.3 us with other tenants,
  +0.93 to +1.25 us alone on the device after the merge); a child forked after
  `cuInit` exits at once (the first library hung it). **Reasoned**: the
  drop count per call rather than cumulative on other CUPTI releases. The
  brief's "within the calibration's error" does not hold and should not
  (dispatch and drain inside a kernel's duration), now a decision. The probe
  question: `omn list` was right; `cupti` was available and nothing
  collected it.
- **device** (`w19-device.md`): every NVML run states each device's clock
  lock, application clocks, persistence mode and clock event reasons; a range
  in the performance levels reads "not shown locked"; readings the driver had
  not refreshed are counted (`driver_sample: repeat`), the default poll stays
  1 s; NVML's cost per call kind (persistence 6-9 us to the draw 2.8-7 ms);
  **reading the energy counter often makes it under-count**, for every reader
  of the board (23 W idle read at the ends, about 3 W read every 10 ms).
  **Shown** on the RTX 2080 Ti; **reasoned**: that a `-lgc` lock shows in the
  levels (the run that decides is privileged, below).
- **core** (`w19-core.md`): `view/` (`omn-view`, ratatui 0.29), linked into
  `omn` as `top`: live on the machine, a pid, a cgroup or a launch, `-f`,
  `--replay`, `--gallery`, `--against`, `--headless`, `-o`; the live session
  the union of the views' needs, each need asked alone and drawn as its
  refusal when it cannot be served; the view watching the runner through
  `collect::Escalate`; the trust style; the view's own cpu in a corner and
  taken out of the collectors'; seven fixtures recorded through `omn top
  --headless -o` by `tests/view_fixtures/record.sh`, and the snapshot
  harness with `.ansi` twins. **Shown** on a pty: a replay, the keys, a live
  run quit at two seconds ending every collection; the view 1.1-1.8 % of a
  cpu on a replay; the hand-over not what makes reads late. **Reasoned**:
  "unused, it costs nothing" (no collector or runner path changed; startup not
  timed). Found and fixed beside the view: `Gated`'s drop released an
  unreleased child before killing it (`src/collect/launch.rs`).
- **profiler** (`w19-profiler.md`): `gpu.counters` through CUPTI's range
  profiler in kernel replay, each launch's window witnessed by
  `sm__ctas_launched.sum` against its grid; the new **`gpu.profile`** through
  PC sampling, resolved to source lines; both sharing `gpu.kernel`'s socket,
  both carrying the clock state. **Shown** (`tests/gpu_counters.rs`, 8): DRAM
  bytes reproducible on a copy and not dominated on a spin; instruction counts
  bit for bit on both; the known hot loop's line 95.9 % of the samples;
  samples against the self-timing kernel's cycles 1.0006 and 1.0014; counters
  beside kernel times refused before the program runs. **Found**: another
  process's kernels added the same 283 million instructions to every launch,
  so a wrong count repeated -- what the witness is for. **Reasoned**: pairing
  under several launching threads, more than one device.
- **bench** (`w19-bench.md`): `examples/bench/gpu.c` (three regimes),
  `tests/bench_gpu.rs`, the record in `docs/bench.md`; CUPTI costed against
  the library loaded and disarmed. **Shown** (`gpu-1791099533`, on a shared
  machine): `gpu.kernel` +67.05 % launch-bound (process cpu +82.75 %, the
  launching thread +66 %, 1.3 us a launch), +0.81 % compute-bound, +0.14 %
  memory-bound -- band `extreme`; `gpu.api` +11.31 % -- band `high`; `nsys`
  +74.71 %; `omn`'s own decoding about 1.3 us a record; the control inside
  the floor. `gpu.counters` and `gpu.profile` are `pending`. After the
  merge, alone: `gpu.counters` +282 934 % launch-bound (`extreme`), and
  `gpu.profile` unmeasurable, a double free in `libomn_cupti.so` at the
  target's exit (filed).
- **system** (`w19-system.md`): one box, every resource on one time axis,
  colour for trust only, a measured zero and an unmeasured cell drawn apart,
  the process list, the cost corner.
- **flame** (`w19-flame.md`): two icicles, on and off cpu, folded chains,
  zoom, search, threads, and `--against` judged by the triage's arithmetic
  restated (38 of 378 functions up beyond 3σ absolutely, none in shape).
- **ledger** (`w19-ledger.md`): one process's cost sheet, each line with what
  knowing it cost and its trust, the energy share labelled an estimate, two
  runs line for line.
- **request** (`w19-request.md`): every request as a bar of six states and
  the selected one's waterfall; on the vertical case it names each injected
  cause unprompted, and its block requests are the join engine's to the
  nanosecond.
- **timebar** (`w19-timebar.md`): each decomposition's table as a bar with
  `omn latency`'s suspect by its rule, held to the triage by
  `tests/view_timebar.rs`; a per-process cpu total drawn as a bracket per
  thread.
- **heatmap** (`w19-heatmap.md`): latency over time from `raw` events, a
  kernel row spread over its drain window; `sqlite-commits` in three regimes
  pulsing once per commit.
- **lanes** (`w19-lanes.md`): one lane per thread rebuilt from switch-outs
  and wakeups, a lane per GPU stream; on its own recording the main thread's
  132 ms `cuCtxSynchronize` block lies under the 132 ms kernel it waited for.
- **wake** (`w19-wake.md`): wake chains folded into a tree with each link's
  hold and quiet; the HTTP server's handler holding the main thread 273 ms a
  listing.
- **flows** (`w19-flows.md`): bytes as two Sankey bands across a kernel-layer
  junction; on its own file-server recording every file's bytes exact and
  loopback 2.00x the socket.

Every view was judged on its snapshots and `--headless`: no view stream had a
terminal, and only core drove one (under `script`). Each view's verdict is in
`docs/view/<view>.md` and gathered, with how to see each one, in
`docs/view/gallery/README.md`, for the user to keep or drop.

**At the merges**: phase A's first `make check` was red at
`family_sched::a_launched_programs_children_are_followed_and_wait_what_their_schedstat_says`
(2.5 % against 2 % allowed); the `--no-fail-fast` after it, **2363 passed, 3
failed, 11 ignored** -- `cases` (in the tally) and
`scenario_sched::a_lock_held_for_a_known_time_...` green alone, the ring-loss
gap red again -- and clippy red on the device stream's code, fixed in
`91a0cef`; the second `make check` had 1895 passed and stopped at the
ring-loss gap. Phase B's `make check` was red at
`portability::every_cell_covers_the_whole_catalog` (the matrix cells predate
`gpu.profile`); the `--no-fail-fast` after it, **2545 passed, 3 failed, 12
ignored** -- `page_faults` (in the tally, green alone), the ring-loss gap and
the stale cell -- and after `c6bd2b8`, run by hand step by step, 2546 passed,
the same two red, clippy and fmt clean. **`make check` stays red on this
machine until the matrix is swept again** and the ring-loss gap is closed.
Both merges pushed (`4aef377..91a0cef`, `91a0cef..c6bd2b8`); the fourteen
worktrees' `target/` removed. `omn list`: 95 usable here, 99 sources with a
collector, 111 of 206 backends. The decisions are in `DECISIONS.md`,
"Settled in the nineteenth wave", with two new open lines.

**The twentieth wave is merged**, all fourteen streams, and
`docs/briefs/wave-20-left.md` keeps its brief: everything the list after the
nineteenth wave held (its items 2 to 11) so that the next wave can compare
`omn` with the reference tools on a tool with nothing else pending. Two
phases; each stream merged `--no-ff` as soon as it was done and reviewed while
merged, its review's fixes a second merge. Phase A, eleven streams: `df317ce`
net (`7e1c2de`), `25b5618` io, `520d3f4` sched (`9417acd`), `978817a` triage,
`03df991` cpu (`e654f86`), `db6ae54` gpu (`4470dcc`), `2a1d1cf` workloads
(`92a8d72`), `25aca8d` budget (`29abbc4`), `23b1200` shared (`785a335`),
`efc5b85` view (`ab4778d`), `777d6dd` rings (`dce14b9`). Phase B, three:
`baf39f3` process (`17521b4`), `4f25509` record (`25632bd`), `e0550cc` docs.
Then the merge's fixes, `96b4741` and `d661d67`. Each stream's handoff is
`docs/fixes/w20-<stream>.md`:

- **gpu** (`w20-gpu.md`): **`gpu.profile` no longer aborts a native CUDA
  program at exit** -- CUPTI shares one `functionName` across every record
  of a function, later drains' included, and the library freed it after each
  drain; the names are now freed once, after every context's last drain. A
  target that dies inside `libomn_cupti.so` after its exit handler began (a
  new `OP_EXITING` frame) is said as the tool's failure in a `degraded:` line,
  not only a lost read; counters beside kernel times or PC sampling are
  refused before the launch, exit 2, decided on the resolved backends; no
  unwinding warning for `gpu.profile`; the run's environment carries the GPU
  clock states when a `gpu` source is asked; `gpu.engine_busy` takes `raw`.
  **Shown**: the abort 1 of 1 before and 0 of 3 after, the new native test
  (`examples/bench/gpu.c` under the three inside sources) failing on the old
  code; the refusal exiting 2 with the program never started; `kernel.rs`'s
  read path profiled at 1.4 to 1.8 us a record, of which the event model's
  copies and allocations (about twenty strings an event) are most, not
  CUPTI's decoding. **Reasoned**: no lighter activity kind is a fallback (the
  other kernel kind serialises the program's kernels). **Not done**: `omn`
  still exits 0 when the tool killed its target (no channel from a collector
  to the run's outcome); `gpu.temp`'s band waits on the next GPU bench; the
  bench's schedule.
- **rings** (`w20-rings.md`): **`cpu.profile`'s `losses_by_ring` carries each
  ring's throttles beside its drops** (`src/perf.rs` splits them per ring as
  it split the drops), so `emit.rs` places them and the runner test passes:
  **shown** 6 of 6 at load 10 to 11, each series holding its own ring's
  `(dropped, throttled)` -- the light task's ring is throttled too under load,
  so the old "not on every series" assertion could not hold. `unwind`'s
  stripped build's second shape is **aliasing**: one sample a 973 us batch
  at a drifting phase put 229 of 299 samples in one branch at the batch's
  period; sampled every 61 us, under the shortest branch, 10/30/40/20 % built
  read 10/30/40/20 %. **Reasoned**: that aliasing is what the two field
  sightings were. **Not done**: the share tests' weaker aliasing (a
  pseudo-random pad in the workload).
- **triage** (`w20-triage.md`): one spelling of a limit in the JSON
  (`cpu-queue`, `other-block`; the old one still read); **the tool's own eBPF
  frames never the named suspect** (samples under `bpf_trace_run*` and the
  like set aside, counted and said as the tool's cost); **a thread blocked on
  a connection beside an otherwise empty block is followed** (`waiting`,
  recognised by its send/receive frames); a followed child carries **the rest
  of the job** (`followed.job`); a lossy lower row that could overtake the
  largest at its source's loss rate undecides the verdict. **Shown** on the
  triage's own workloads (`workload job`, `serve.py`), 29 of 29 in
  `tests/triage.rs`, no answer naming an eBPF frame. **Reasoned, and not
  yet so on the cases**: `http-server-stdlib`'s slow client and
  `make-sqlite`'s and `gcc-shell`'s job numbers still list their findings
  (`cases/*/case.yml`), and `cases` passes with them listed. **Not done**: the
  job as one decomposition (needs `ppid`); a C program's send through an
  unnamed libc frame; the tail's verdict under the lossy rule.
- **budget** (`w20-budget.md`): the symbol table `mmap`ed and read lazily
  (**shown**: 203 to 217 ms of cpu to load the 192 MB debug `omn`, 79 to 81
  after), kallsyms' usability decided once rather than per kernel frame (1.73
  ms a frame before, at paranoid 2); the in-target meters read before the
  stops; **a one-time load weighed apart from the rate**
  (`overhead.one_time_cpu_ns`: a debuginfod profile's first check from 142-152
  % down to 45-51 %); a session refused at resolution names every request;
  the launched program's stderr a file of its own (`OMN_PROGRAM_STDERR`,
  `program_stderr` in `run_session`) and `stopped_by_budget` in its answer;
  `omn` the subreaper of what it launches, every orphan ended at the end of
  every run (**shown**: a 71.7 s hang gone); `docs/mcp.md`. **Reasoned**: the
  meters before the stops (a privileged run below), the one-time share with
  several collectors. The first check is not settled by it (`DECISIONS.md`,
  "Open").
- **sched** (`w20-sched.md`): `sched.wakeup` names both ends (`wakee_comm`,
  `waker_comm`) and an interrupt's wakeup its `irq` or `softirq` and the task
  it was served in (`served_in_tid`, `served_in_comm`); `sched.offcpu`'s
  caveat says it stamps at the drain, as it does; `sched.lock` and
  `sched.futex` follow a process target's children; the two example headers.
  **Shown**: 40 400 hard and 10 731 soft interrupt wakeups named over 79 959
  `O_DIRECT` reads, and the test failing with the collector mutated; a
  child's 500 ms futex wait, 0 events before and one after. **Found**:
  `comm` was always the wakee's -- the nineteenth wave's timebar finding was
  a launched program before its `exec`, withdrawn. **Reasoned**: timer and
  IPI wakeups carry no `irq`; the four interrupt programs' cost is
  unmeasured.
- **net** (`w20-net.md`): `net.socket` keys its route's interface
  (**shown**: every loopback row `iface == "lo"`), `net.bytes` takes `raw`
  (rx and tx kept: **shown**, `bytes == rx + tx` over 8 MiB),
  `net_qdisc`'s namespace through `member_offset`, and `net.request`'s
  never-ended, unborn and replaced counts as a struct the notes are written
  from. **Not done**: those counts as fields of the result (no slot in
  `Results`, no hook in `Collector`), the bytes per request and the start of
  the longest sleep (`src/bpf/request.rs`, io's, not done there), and
  `net.bytes` as one series per direction. **Reasoned**: a socket in another
  namespace carries no interface.
- **cpu** (`w20-cpu.md`): **`cpu.oncpu_time` through `bpf:raw_tp` on
  `sched_stat_runtime`**, one row per thread, the scheduler's own increments:
  **shown** exactly equal to the thread's `schedstat` over a window whose
  edges find it blocked. A first version on `sched_switch` read nothing for a
  thread never switched out, caught by a scenario and replaced before the
  commit. `raw` on `cpu.oncpu_time` and `power.energy`. A privileged binary
  now reads one series per thread, and a launched program's children: the
  merge's two test regressions below.
- **io** (`w20-io.md`): `blk_devs` on `io.fs`, `io.fsync` and `io.pagecache`,
  a filesystem's block devices as `io.bytes` keys them (**shown** on this
  btrfs); `io.inflight`'s zero on 6.17 explained from the kernel's source
  (a partition's in-flight count was never the disk's until 7.2;
  **reasoned**, the CI's 6.17 runner is what shows it); merges measured, not
  predictable from adjacent bios (16 reads, 2 requests, 1 merge, 3 of 3), so
  no test holds a count; `raw` on `io.bytes` and `io.syscall` checked, already
  served. **Not done**: a direction key, and `net.request`'s four storage
  fields net asked for.
- **shared** (`w20-shared.md`): `footprint.rs` reads again after an ESRCH
  from a running process; the `cli` tests' `/tmp` leak closed; the
  memory-limit test's cgroup named per call; an `equivalent` backend cheaper
  than what it stands in for refused by the catalog loader; **`omn join -f
  <rules> <results>`** over a stored run, with `tier:` in `man/omn.1`
  (**shown** answering field for field what the agent tier did). **Not done,
  routed**: `assemble`'s query-tier skip and `sources_read` (`src/main.rs`),
  `NamedBy` in `Results::stacks` (`src/emit.rs`), the executable of each task.
- **view** (`w20-view.md`): **the links** (a history, `⌫` and a right click
  back), **the mouse** (each view's regions, a click selects and a second goes
  in, the wheel, `M` to give the terminal its selection back, SGR through
  tmux), `process` registered and the first screen of a run with a target;
  `View::takes_text`, `draw_with`, `units::approx`, `as_live_until`'s
  sentence, the flame's diff held to the triage's, the README's `why` column,
  a scale-aware dimming; in `results.json` each timeline's `first_ts_ns`,
  each result's `value_attr`, `row_stamp` and its drains' drops with their
  times. **Reasoned**: the mouse in a real terminal and through tmux. **Not
  done**: a ledger line of blocked time (so a ledger line cannot open
  `timebar`), `process` first on a live `--pid` or launch, the live run's
  `value_attr`, `row_stamp` and drains (`src/main.rs`).
- **workloads** (`w20-workloads.md`): a program and a truth manifest per view
  and for `process` (`view/workloads/`), Python reading its parameters from
  its manifest, the request one the vertical server in C; 14 tests in
  `tests/view_workloads.rs` holding each source to its manifest. **Shown**:
  ten consecutive green runs under the other streams' load.
- **record** (`w20-record.md`): **one re-recording**, 20 fixtures, 22 MB, in
  2 min 37 s on a binary with its capabilities (`record.sh` refuses one
  without); every snapshot redrawn and one per view after a click;
  **`tests/view_truth.rs`, each view held on screen to its workload's
  manifest**; a re-recording of the nine workloads shown drawing the same
  screens. **Found and fixed**: a machine run claimed a target ("what task 1
  and 3120 more cost", "target 753 % cpu"), and the view's mirror lacked
  `one_time_cpu_ns`. **Not done**: `--against` and focus snapshots in the
  standard set.
- **process** (`w20-process.md`): the `process` view, nine tiles, each a
  number and a colour and each opening its view; the colour is the suspect's
  state on the tile that explains it, never a threshold; its recording
  `docs/view/gallery/process-run/`. **Shown** against its manifest: `crunch`
  first, the rss over the 32 MiB held, the socket bytes exact. **Reasoned**:
  its suspect reads `sleep` where the workload holds a lock 0.25 s (the
  timebar does not split a `sched.futex` summary out of sleep).
- **docs** (`w20-docs.md`): `CATALOG.md`'s cost column written as the YAML's
  chains (49 rows) and its sched section; `proc-not-built.yml` exits 3, or 1
  where tracefs is root-only; `GUIDE.md`'s latency session; the perf map in
  `docs/unwind.md`; `judge_band` reads a dilation within the workload's
  resolution as zero; the cases ask with no budget and read the program's
  account from `program_stderr`; two flaky bounds fixed (`scenario_mem`,
  `page_faults`). **Found**: `scenario_runtime`'s thread counts are a uprobe
  hit carrying another thread's tid, shown on a second reproduction, its
  cause in the kernel reasoned.

**At the merge** (`docs/open-findings.md`, "The twentieth wave, coordinator,
after the merge"): three `make check` runs and one `--no-fail-fast`. The first
`make check` stopped at `cases` (1665 passed): the triage's spelling fix met
two cases still listing it as an open finding, which the test refuses
(`96b4741`). The `--no-fail-fast` after the second, **2656 passed, 4 failed,
13 ignored**: three merge regressions red alone too -- two tests read a
launched program's `cpu.oncpu_time` as its own process, now that `bpf:raw_tp`
follows its children one series per thread, and a heatmap test whose
re-recorded fixture stamps the last drain past the run's end (`d661d67`) --
and the stale matrix cells. The third `make check` (1732 passed) stopped at
`cross_machine`'s memory-limit test (64 of 42 100 pages outside the program's
intervals), green alone, filed; clippy and fmt then run by hand, clean.
**`make check` stays red on this machine until the matrix is swept again.**
Nothing pushed before this commit. `omn list`: 95 usable here, 99 sources with
a collector, 112 of 206 backends. The decisions are in `DECISIONS.md`,
"Settled in the twentieth wave".

**The twenty-first wave is merged**, all seven streams, and
`docs/briefs/wave-21-seams.md` keeps its brief: what the twentieth wave's
streams asked of each other, regrouped by the file that has to change, and
the `process` view redone as the user meant it. Five streams at once,
`collectors` once `data` was merged, `record` once everything else was; each
merged `--no-ff` as soon as it was done and reviewed while merged, its
review's fixes a second merge: `d3e2285` docs (`c8aa662`), `464d59b` process
(`6e419ed`), `ef57e2b` views (`20ee3a9`), `2ba6626` run (`a44eec6`),
`320c2aa` data (`32eca2e`), `6f5d1a6` collectors (`ed9140c`), `acb1daf`
record (`309c3e4`). Then the merge's fix, `65b3ba4`. Each stream's handoff is
`docs/fixes/w21-<stream>.md`:

- **run** (`w21-run.md`): **a collector that broke the program it measured
  fails the run** -- `Collector::failure()`, every result written first, then
  `error: <sentence>` and exit 1, from `omn`, `record` and `omn top`; the GPU's
  inside readers answer it from `died_in_exit`'s condition. `assemble` clones
  only what a join reads, a query-tier rule left to the stored run. **`omn
  top` opens on `process`** with a launched program or `--pid`, and the live
  results get `value_attr`, `row_stamp` and drains before the first events.
  `--program-stderr FILE`, refused beside a command that launches nothing;
  the stop's subreaper in the manual. A test holding `CATALOG.md`'s Cost
  column to the catalog's chains (87 rows). **A launched program's
  denominator takes in its adopted orphans**, `omn`'s own helpers set aside.
  **Shown**: exit 1 with one failure and 0 without; the program's stderr in
  the flag's file; the cost-cell test failing on a mutated row; the
  denominator, 3 of 3 runs moved onto a worse backend before and 0 of 3
  after, for about 2 ms of the tool's cpu when an orphan exists. **Reasoned**:
  the GPU's failure on a real CUDA program (no GPU run), `omn top` opening on
  `process` in a real terminal.
- **data** (`w21-data.md`): **`KeyKind::Dir`** over a closed set of
  directions; **`Collector::fields()` into `Results::fields`**; stack rows
  keyed by the request's keys (`StackCount::series`, the flame one tower per
  series, `omn diff` summing them back); a refused target's reason on its
  `targets:` line, the caveat search gone; **each frame's namer, the perf
  maps and each task's executable beside the stacks** (`named_by`,
  `perf_maps`, `executables`). **Shown** by unit tests and `omn trace
  gpu.kernel --pid 1`'s refusal quoting the line. **Found in its own make
  check**: `omn diff` compared two runs' tids as different stacks, fixed in
  `src/triage/diff.rs`.
- **collectors** (`w21-collectors.md`): **`net.request`'s `rx_bytes`,
  `tx_bytes` and `holder_since_ns`**, `rx_bytes` from the first byte's arrival
  (a birth-to-end count read 0); its four counts as `fields()`; **`io.bytes`,
  `io.iops`, `io.merges` and `net.bytes` one series per direction** under
  `dir`, unchanged without it; the polled `io` counters' baseline taken
  before the release; `cpu.oncpu_time` on `fs:proc`'s caveat and the
  interval the reap took away counted as a loss; `cpu.profile`'s
  provenance (`named_by`, `executables`). **Shown**: every `rx_bytes` exactly
  the client's request line and every `tx_bytes` the worker's `sent=`, in a
  scenario test that builds the server with numbers of its own and fails on a
  decoder one byte off; one series per device and direction; `executables`
  naming `/usr/bin/python3.14`. **Found**: a flattened `named_by` did not read
  back from JSON, so every profile with namers failed to load in `omn diff`
  (`139450c`). **Not done**: the closing read of a zombie (the runner's),
  `perf_maps` (the unwinder keeps its symbolizer private).
- **process** (`w21-process.md`): **`process` redrawn as the system view for
  one process** -- one header (`pid · threads · life · omn costs`), one line
  per resource, each a sparkline over the program's life with now, mean, peak
  and share, the suspect under them; each line opening its page by key and
  by click; the usage gradient where a capacity exists, one neutral hue and
  a sentence where none does; a source not asked says so in place, dimmed,
  never zero; a machine run claims no target. **Shown** against the
  manifest's phases on `process-run` (CPU, memory, network exact, disk),
  every page by key and click. **Reasoned**: the gradient on a real terminal;
  the suspect still `sleep` on a workload that holds a lock 0.25 s.
- **views** (`w21-views.md`): **the usage gradient** in `trust.rs` (six
  256-colour steps, a neutral hue), `system` coloured by how full each
  resource is and asking `io.bytes`/`net.bytes` as `raw`; **a ledger line of
  blocked time that opens `timebar`**; `ledger::reading` and `flame::hottest`;
  **the timebar splitting `sched.futex` out of the sleep**; the heatmap's
  drops at their drains; flows naming a file's block device; `Results::fields`
  and a typed `Lifetime` in the mirror, the request view counting from fields.
  **Reasoned**: the ramp on a light terminal and in 256-colour tmux; a placed
  drop on a real recording (none holds one).
- **docs** (`w21-docs.md`): every twentieth-wave request to `CATALOG.md`,
  `docs/unwind.md`, the proc documents, `docs/bench.md`; the vertical server
  holds 1024 requests and refuses more; the cases read `followed.job` where
  they say the wait is for a child; `cheaper_equivalents` gone from the test;
  **a replaying GPU arm measured at 2000 launches** beside a bare run and a
  control of that size. **Shown**: the cases twice, green on the second
  (`gcc-shell`'s finding closed on a loosened comparison, an upper bound on
  the MCP way). **Reasoned**: that the GPU step fits in about fifteen minutes.
- **record** (`w21-record.md`): **every fixture re-recorded** on the merged
  tree, `process-run` among them with `io.latency`; 477 snapshot files
  re-blessed; `process` in the gallery, "yes (2026-10-04)"; `view_truth`
  holding `process` to its manifest. **Found**: `process-run`'s Disk line is
  one 128 KiB request short of the file every time, reconciled to the byte --
  a block request kblockd's `kworker` dispatched is charged to it, not to the
  program (filed).

**At the merge** (`docs/open-findings.md`, "The twenty-first wave,
coordinator, after the merge"): one `make check`, red at the first failing
binary, then `--no-fail-fast`: **2691 passed, 3 failed, 13 ignored**. Two were
regressions where streams met, fixed in `65b3ba4`: the gallery test looking
for the frame the new `process` no longer draws, and the view's mirror
dropping the provenance fields data added and collectors filled. The third,
`the_runner_closes_the_window`, is in the tally and passed alone. **The
second `make check`: green, 2694 passed, 0 failed, 13 ignored, 64 test
binaries**, clippy and fmt clean. `omn list`: 95 usable here, 99 sources with
a collector, 112 of 206 backends. The decisions are in `DECISIONS.md`,
"Settled in the twenty-first wave".

**The twenty-second wave is merged**, all twelve streams, and
`docs/briefs/wave-22-replaces.md` keeps its brief: **`omn` against every
tool it can replace**, tool by tool -- for each common use, what `omn`
serves, whether it gives the same numbers on the same workload within a
declared tolerance, and what is missing. One stream for the shared runner,
eleven for the families of tools, all at once; each merged `--no-ff` as soon
as it was done and reviewed while merged, its review's fixes a second merge:
`ff055c8` reference (`6cb1225`), `9d367ef` perfstat (`62c8e84`), `ad36ec7`
perfrecord (`61bc1c0`), `ccf79d1` perfsched (`aaa2102`), `9de3c76` bpftrace
(`5ef23a9`), `5370f11` sysstat (`e5a1205`), `928e949` top (`92fc0ae`),
`51e3822` heap (`438afca`), `2c1bddc` net (`ea97e9a`), `7fe0d53` power
(`b7425c5`), `45d886a` gpu (no review round), `c6935e0` io (`f578ad5`). Then
the merge's fix, `bc9f9a5`. **Nothing under `src/` changed**: a gap in `omn`
was filed, not fixed. Each stream's handoff is `docs/fixes/w22-<stream>.md`,
each family's verdict `docs/replaces/<family>.md`, and `docs/replaces.md`
says across families what is replaced fully, in part and not at all.

**The one thing to know first**: the families wrote their pairs while the
runner was not on `main`, each to a schema of its own, and **no pair has yet
compared a number through `tests/reference.rs`**. 117 pairs in eleven
files; `pairs_bpftrace` passes because its ten pairs skip on `sudo -n`,
`pairs_net` because its nine declare a `gap:` pointing at the family's own
harness; the other nine families fail the runner (paths it does not parse,
`{pid}` outside `attach`, values looked for outside `results[...]`, `omn`
command lines `omn` refuses, fields it ignores). Every "shown" below was
shown by the family's own harness -- an ignored test, a `run_pairs.py`, a
`mod runner` -- or by hand, on the YAML's exact commands.

- **reference** (`w22-reference.md`): **the runner**
  (`tests/reference/runner.rs`): every `catalog/references/*.yaml` loaded,
  `omn` and the tool run `separate` (alternating, medians over `runs`) or
  `attach` (one stopped workload both observe by `{pid}`), `{tmp}`, `{out}`,
  `{pid}`, a `report:` command, a dotted path with `*` and `[field=value]`
  filters summed, `{rel, abs}` with the larger allowed, `omn_scale` and
  `reference_scale`, `key: "*"`; outcomes pass, skip (the machine's reason
  only), gap and fail. `omn`'s exit 1 a skip only when nothing was written
  and the workload succeeds alone; a tool's refusal a skip only when its
  stderr says so and the workload succeeds alone. `families_are_whole`,
  `every_pair_is_well_formed`, `every_row_names_a_pair_or_why_none`;
  `catalog/references/README.md`; `docs/replaces.md`; three skip lines in
  `.github/expected-skips.txt`. **Shown**: the runner's unit tests (8) at the
  merge. **Reasoned**: every family's pairs through it.
- **perfstat** (`w22-perfstat.md`): 13 pairs, 2 parsers, 19 rows.
  **Shown** by `run_pairs.py`, 39 runs, 0 failing: instructions within
  0.01 %, cycles, branch and cache misses, context switches, migrations,
  page faults (omn 3 to 7 more, every run), task-clock, IPC per process;
  cycles and context switches `--system`, instructions per cgroup, the same
  run with perf wrapping `omn`; instructions per on-cpu ns by `--pid`, omn
  0.5 to 1 % above every run. **Reasoned**: that the runner reads
  `same_run`, `setup` and `omn_jq` as `run_pairs.py` does.
- **perfrecord** (`w22-perfrecord.md`): 7 pairs, 7 parsers, 8 rows.
  **Shown** by its ignored test: self shares of three C functions and of a
  stripped library within 0.05 against `perf record`/`report` and `perf top
  -p`, `perf mem`'s mean load latency within 10 %, the false-shared line
  `0x4040c0` first in both `perf c2c` and `mem.sharing`. Two untouched pairs
  fell outside once each under load in the review round. **Reasoned**:
  inclusive shares, `omn top --view flame` against `perf top`.
- **perfsched** (`w22-perfsched.md`): 9 pairs, 6 parsers, 14 rows.
  **Shown** by hand: `strace -c` 5000 `getppid` exact; a 10 ms sleep within
  0.2 % of `strace -c -w` and `strace -T`. **Reasoned**: the six `perf`
  pairs (tracefs is root's here), on four captures written from perf's
  documented layouts; the `perf sched latency` delay pair removed in review,
  the two quantities differing.
- **bpftrace** (`w22-bpftrace.md`): 10 pairs, 9 parsers, 25 rows. **Shown**:
  `omn`'s side of each pair exact or within its startup against what the
  workload fixes. **Reasoned**: every tool-side number; the 13 captures are
  format fixtures with synthetic numbers. Its review round was committed in
  stream reference's tree by a `git stash` swap (`c2194ab`), the lesson in
  the handoff.
- **sysstat** (`w22-sysstat.md`): 13 pairs, 8 parsers, 26 rows; the
  sysstat tools were installed by the time the pairs ran. **Shown** by its
  own test: `numastat -p`, `pidstat -r`, vmstat's busy share, PSI, `mpstat`,
  `iostat -c`, `sar -u` within their bands; `pidstat -w` 66 685/s against
  944/s, held as a `known_defect`; the poll's leading event held by its own
  test both ways. **Reasoned**: swap and memory pressure agreeing beyond zero.
- **top** (`w22-top.md`): 11 pairs, 5 parsers, 21 rows. **Shown** by
  `run_pairs.py`, 33 runs: RES, `%CPU`, `%Cpu(s)`, `pmap -x`/`-XX` RSS, PSS
  and USS, smem's three (Debian's script run in place), ten of them one run
  through `onpid.py` (deleted in the twenty-third wave: `run: attach`);
  TIME+ on alternate runs within 30 ms. **Reasoned**: smem's PSS deficit
  being its own python3.
- **heap** (`w22-heap.md`): 9 pairs, 4 parsers, 17 rows. **Shown** by a
  scratch emulation of the runner: dhat's total, blocks, `t-gmax` and
  `t-end` and massif's peak equal to the byte; heaptrack within 0.2 %.
  **Reasoned**: memleak (root; a format fixture only).
- **net** (`w22-net.md`): 9 pairs, 4 parsers, 14 rows. **Shown** by
  `net_pairs_against_the_tools`: `ip -s link` bytes, packets and errors,
  `nstat`'s listen overflows, drops and retransmits, `ss -ti`'s rtt, eight
  exact. **Reasoned**: tcpdump's count (root and `CAP_NET_RAW`).
- **power** (`w22-power.md`): 7 pairs, 5 parsers, 11 rows. **Shown** by
  hand, the tool wrapping `omn`: package energy within 1.9 to 3.1 % of
  `perf stat -a` and `turbostat --no-msr`, busy frequency within 0.1 %, idle
  residency within 0.02 s per cpu, `sensors` equal. **Reasoned**: the RAPL
  files and cpupower's Mperf (root).
- **gpu** (`w22-gpu.md`): 21 pairs, 6 parsers, 20 rows. **Shown** by its
  ignored test, two full runs (19/21, then 20/21, each miss passing on its
  own reruns): launches, copies, driver calls, instructions, blocks, L1
  bytes and per-process VRAM exact against nsys, ncu and nvidia-smi; kernel
  time within 0.3 % launch-bound, DRAM bytes within 0.4 %, power, clocks,
  utilisation and temperature the same run within their bands.
  **Reasoned**: nsys and ncu beside `omn`'s CUPTI library in one process.
- **io** (`w22-io.md`): 7 pairs, 2 parsers, 12 rows. **Shown** by hand:
  `io.latency`'s count equal to fio's three times on one run, its mean 18 %
  below fio's `clat` (the layers above the block device), `io.bytes` and
  `io.iops` machine-wide equal to fio's. **Reasoned**: the three `iostat -dx`
  pairs (iostat absent when the stream ran; installed now), on a capture
  written in sysstat's JSON layout.

**At the merge** (`docs/open-findings.md`, "The twenty-second wave,
coordinator, after the merge"): `make`, then `make check`, red at
`tests/cases.rs`; then `--no-fail-fast`: **2805 passed, 15 failed, 15
ignored, 65 test binaries**. Two were a regression where streams met, fixed
in `bc9f9a5`: stream reference's review fix (`scenario_cpu` and
`scenario_proc` copying `catalog/references/` as a file, "Is a directory")
had stayed in a dangling stash when two streams' `git stash pop`s crossed.
Eleven are `tests/reference.rs` (nine `pairs_<family>` and the two data
checks), filed above, not a merge fix. `cases`'
`sqlite3-durability` is the eighteenth wave's load-sensitive case and passed
alone. `cross_machine`'s memory-limit session **failed twice inside full
runs** (18.3, then 32.7 MiB of the 64 MiB the cgroup had to write back
reached the device in the program's window) and passed alone between them:
filed, for its cause to be read before it goes on the tally. **The second
`make check`: red** at `tests/cross_machine.rs`, 1750 passed and 1 failed,
the sixteen binaries before it green. `omn list`: 95 usable here, 99 sources
with a collector, 112 of 206 backends -- unchanged. The decisions are in
`DECISIONS.md`, "Settled in the twenty-second wave".

**The twenty-third wave is merged**, all thirteen streams, and
`docs/briefs/wave-23-runner.md` keeps its brief: **every verdict through the
runner** -- the twenty-second wave's pairs, rewritten to one schema and run
by `tests/reference.rs`, and nothing else. Two phases: the runner (with
stream cross beside it) merged first, then the eleven families at once, each
merged `--no-ff` as soon as it was done and reviewed while merged, its
review's fixes a second merge: `a14cf09` runner (`926adc7`), `f42b3e8` cross
(`4eaeb86`), `e95cd96` heap, `c0abafa` power (`bef19fd`), `4dceb47`
perfsched, `8c06adf` bpftrace, `a87c68f` sysstat, `c090225` net (`a531e7a`),
`d25f5e1` perfstat, `7bf20e5` gpu (`f4d7321`), `a6480bf` io (`48233eb`),
`bf5b2bf` top, `d632384` perfrecord (`1edb677`); then the full suite's
filing, `f987c8d`. **Nothing under `src/`, `view/` or `catalog/*.yaml`
changed**: every gap the runs found was filed. Each stream's handoff is
`docs/fixes/w23-<stream>.md`.

**The one thing to know first**: every pair now runs through the shared
runner, and **113 pairs in eleven files give the runner's own verdict**. The
families' harnesses are gone (`run_pairs.py` in perfstat and top, top's
`onpid.py`, net's `mod runner`, the ignored tests of gpu, perfrecord and
sysstat); `every_pair_is_well_formed` and `every_row_names_a_pair_or_why_none`
pass; no pair declares a `gap:`. The tally the runner printed in the
coordinator's quiet run (`cargo test --test reference -- --nocapture`, at
`1edb677`):

| Family | Held | Skipped | Gaps | Known defects | Failed |
|---|---|---|---|---|---|
| bpftrace | 0 | 10 | 0 | 0 | 0 |
| perfsched | 3 | 6 | 0 | 0 | 0 |
| power | 5 | 2 | 0 | 0 | 0 |
| heap | 8 | 1 | 0 | 0 | 0 |
| perfstat | 13 | 0 | 0 | 0 | 0 |
| net | 7 | 1 | 0 | 0 | 0 |
| io | 7 | 0 | 0 | 0 | 0 |
| top | 10 | 0 | 0 | 0 | 1 |
| sysstat | 12 | 0 | 0 | 1 | 0 |
| perfrecord | 5 | 0 | 0 | 0 | 0 |
| gpu | 21 | 0 | 0 | 0 | 0 |
| **all** | **91** | **20** | **0** | **1** | **1** |

Every skip is the same one: `sudo -n` does not grant root here (item 2).
The known defect is `pidstat -w` against `sched.switches`' counted rate. The
failure is `top-cpus`, which held alone at the edge of its band (below).

- **runner** (`w23-runner.md`): the schema as code. `compare.omn_parse`
  (a family's parser applied to `omn`'s output), three one-run shapes
  beside `separate` and `attach` -- `alongside` (a name for `attach` with no
  `{pid}`), `tool_wraps_omn`, `omn_launches_tool` (the tool's own status
  read through a pidfd, Linux 6.15) -- `setup:`, `requires_setting:`,
  one-sided and `log2_buckets` tolerances, `known_defect:`, a tally line per
  family and `reference_tally`; a field the runner does not read refused per
  pair. **Shown**: 21 unit tests on stub tools; heap's pairs through it.
  **Found**: a perf tool wrapping the capable `omn` counts 0 (inherited
  perf events end at the exec of a non-dumpable binary).
- **cross** (`w23-cross.md`): the memory-limit session's two filings
  closed, neither for the cause filed: the volume under `/var/tmp` is btrfs
  on two partitions and the test read one; the window is now launched by the
  session and ended by the program at its drained writeback; the stray
  reclaimed pages were the program's own after its last read; swap-ins on
  `zram0` netted out of `/proc/self/io`. **Shown**: 5 of 5 alone, 6 of 6
  under load, 3 of 3 beside a release build; not red in the end-of-wave suite.
- **heap** (`w23-heap.md`): 9 pairs, dotted paths, no parser of `omn`'s
  output. **Shown**: dhat's total bytes, blocks, `t-gmax`, `t-end` and
  massif's peak to the byte; heaptrack's calls, peak and leaked within
  0.2 %. memleak skips (root).
- **power** (`w23-power.md`): 7 pairs, all `alongside` (`omn` refuses a
  system-scoped source beside a launch, so `tool_wraps_omn` had no
  workload). **Shown**: energy within 0.4 % of `perf stat -a` and
  `turbostat`, busy frequency (omn's `last`) within 1.5 %, idle residency with
  cpu 1 ticked every millisecond (residency is credited only on wake),
  `sensors` equal. RAPL files and Mperf skip (root).
- **perfsched** (`w23-perfsched.md`): 9 pairs, paths summing over `*`.
  **Shown**: `strace -c` exact, the 10 ms sleep within 2 % of `strace -c -w`
  and `strace -T`, medians of five. The six `perf` pairs skip: tracefs is 755
  now but each event's `id` and `enable` are root's, and `perf trace -s` and
  `perf lock -b` load BPF.
- **bpftrace** (`w23-bpftrace.md`): 10 pairs, a parser for the two medians
  with `log2_buckets: 1`, bitesize on the `[4K, 8K)` bucket. **Shown**:
  `omn`'s side on ten real outputs, through the runner's own path code. Every
  tool side skips (root).
- **sysstat** (`w23-sysstat.md`): 13 pairs, three parsers of `omn`'s raw
  polls, five tool captures replaced by real output. **Shown**: 12 held,
  `pidstat -w` a known defect (77 135/s against 931/s).
- **net** (`w23-net.md`): 8 pairs, `alongside`, four parsers of `omn`'s
  output. **Shown**: `ip -s link` bytes and packets exact, errors at zero,
  `nstat`'s overflows, drops and retransmits exact, `ss -ti`'s rtt within
  15 %. **Found**: `net.capture` on `lo` keeps each frame twice (outgoing
  and host); by hand, `omn` 3608 against tcpdump's 1804, equal once the
  outgoing copy is dropped by the parser. The pair skips: the runner wants
  `sudo -n` to grant everything, sudo grants `tcpdump` alone.
- **perfstat** (`w23-perfstat.md`): 13 pairs, paths only. **Shown**: 13
  held three times under load; cycles and task-clock one run with `omn`
  launching `perf stat` (one-sided), `-a` pairs with perf wrapping `omn`, a
  cgroup made by `setup`, `-p` compared raw.
- **gpu** (`w23-gpu.md`): 21 pairs, two parsers of `omn`'s output, the
  harness deleted. The stream saw no driver (the kernel had no `nvidia.ko`)
  and every pair skipped; **in the coordinator's run the driver was loaded and
  all 21 held**: launches, copies, driver calls and ncu's counters exact, kernel
  time within 0.3 % and copy time 3.2 %, power, clocks, utilisation, temperature
  and VRAM within their bands. Its four `omn_*.json` parser captures are
  still composed by hand (item 1).
- **io** (`w23-io.md`): 7 pairs, two `omn_launches_tool`, five `alongside`,
  two parsers (refusing a run that lost records). **Shown**: fio's reads
  exact, bytes and operations within their one-sided bands, `iostat -dx`'s
  `r/s`, `rkB/s` and `r_await` over a window holding the whole job; `io.latency`'s
  mean 28 % below fio's clat, inside the one-sided 50 %.
- **top** (`w23-top.md`): 11 pairs, `run: attach`, the tool released at the
  workload's phase by `at.sh` (written to its `comm`). **Shown**: RES,
  TIME+, `%CPU`, `pmap` and `smem`'s RSS, PSS (omn's `last`) and USS, three
  runs each; smem's PSS one-sided (its own python maps the interpreter).
- **perfrecord** (`w23-perfrecord.md`): 5 pairs, every `omn` side a parser.
  **Shown**: self shares within 0.05 on one attached run, `perf top -p`,
  `perf c2c`'s line, `perf mem`'s mean (277.6 against 275.4 cycles in the
  quiet run; 10-12 % below under load). Fixed in the pairs: a 2 s settle
  (`omn` takes more than 500 ms to attach), `hot.c` phase-locked with a
  997 Hz sampler. **Found**: `mem.latency`'s IBS delivers about 58 % of the
  rate asked and calls 18 % of loads `peer` where `perf mem` calls 6 % a
  cache hit.

**At the merge** (`docs/open-findings.md`, "The twenty-third wave,
coordinator, after the merge"): `make`, then `make check`, red at
`cross_memory` (on the flaky tally, passed alone); then `--no-fail-fast`:
**2848 passed, 6 failed, 14 ignored, 65 test binaries**, each of the six
passing alone once (`budget`, `scenario_power`'s two roads to RAPL, two
`triage`, `unwind`'s DWARF tree, `pairs_perfstat`'s `cpu.branch_misses` at
+5.2 % where 4 % is allowed); clippy and fmt clean. No merge fix. The
reference run above: 159 passed, 1 failed, `top-cpus` (`omn` 0.140 busy
against `top`'s 0.204; alone, 0.076 against 0.120, both times `omn` below):
filed to stream top as a possible one-sided bias. The decisions are in
`DECISIONS.md`, "Settled in the twenty-third wave".

**The twenty-fourth wave is merged**, all sixteen streams, and
`docs/briefs/wave-24-found.md` keeps its brief: **what the comparison found,
all at once** -- "After the twenty-third wave"'s items 1, 3 and 4 split by
the file each lands in, sixteen streams started together in one phase, each
merged `--no-ff` as soon as it was done and reviewed while merged, its
review's fixes a second merge: `63ebae6` unwind (`1007a75`), `0cb4513` run
(`747b26d`), `ae5b924` triage (`e8e6ad6`), `6703f29` lazy (`5d12d25`),
`9e429d0` power (`6fd3eeb`), `6812949` gpu (`baf618b`), `293f4cc` cpu
(`6b4e123`), `e7eaa0b` emit (`36b3d57`), `cee7aa5` mem, `172a033` proc
(`32525bf`), `67dc878` views (`5646f8a`), `8894116` io (`6404005`),
`ea07472` reference (`052d383`), `c1626f9` sched (`a5f0acb`), `f2c39c3` top
(`a08cc27`), `4da72f2` net (`6a8b4c2`); then three merge fixes and the full
suite's filing (`a2ce567`, `6f3c25a`, `367ad20`, `a062341`). Each stream's
handoff is `docs/fixes/w24-<stream>.md`.

**The one thing to know first**: the comparison's defects are closed but one
(heap 1's data line, below), and **sixteen new sources** collect --
`cpu.branches`, `mem.system`, `mem.oom`, `io.util`, `io.vfs`, `net.snmp`,
`net.sockets`, `net.tcp.accept`, `sched.interrupts`, `sched.runqueue`,
`sched.slices`, `power.wakeups`, `gpu.mem_freq`, `gpu.throttle`,
`proc.calls`, `proc.tasks` -- beside a lazy mode (`omn cpu -- prog`), a
machine-wide target bounded by a launched program (`omn trace power.energy
--system -- prog`), several sources on one launch, `-r N`, and `omn top`'s
`system` view listing the machine's processes. `omn list`: **118 sources,
111 usable here, 115 with a collector, 133 of 226 backends** (95, 99 and 112
of 206 before). The reference pairs, alone on a quiet machine
(`--test-threads=1`, at `a5f0acb`):

| Family | Held | Skipped | Gaps | Known defects | Failed |
|---|---|---|---|---|---|
| bpftrace | 10 | 0 | 0 | 0 | 0 |
| gpu | 21 | 0 | 0 | 0 | 0 |
| heap | 8 | 0 | 1 | 0 | 0 |
| io | 9 | 0 | 0 | 0 | 0 |
| net | 7 | 1 | 0 | 0 | 0 |
| perfrecord | 5 | 0 | 0 | 0 | 0 |
| perfsched | 8 | 0 | 0 | 0 | 1 |
| perfstat | 12 | 0 | 0 | 0 | 1 |
| power | 7 | 0 | 0 | 0 | 0 |
| sysstat | 13 | 0 | 0 | 0 | 0 |
| top | 11 | 0 | 0 | 0 | 0 |
| **all** | **111** | **1** | **1** | **0** | **2** |

sysstat is read after `367ad20`: `pidstat-w` failed its `known_defect` in
the run because stream sched fixed the counted rate (933.7/s against 936/s),
and held alone once the marker was removed. The skip is `tcpdump-captured`,
run without `make CAPTURE=1`; the gap is `memleak` (it counts mmap'd memory).
The two failures are filed: `cpu.cycles.system` (-26 to -39 % where -8 % is
allowed: omn's own start and exit dominate a quiet machine, the wave-23 build
fails the same way by hand) and `perf-trace-count` (perf reports lost events
on every run, now refused by its parser). `runqlat` is no longer a gap (run
with `--system` beside the program), and **no pair skips on `sudo -n`** any
more: cpupower, memleak, perf and the bpftrace tools are all granted.

- **run** (`w24-run.md`): a program beside `--system`, `--cpu`, `--cgroup`,
  `--iface`, `--pid`, `--tid` or `--container` is the window, `launch:` in a
  session (shown: energy over a 2 s program equal to `perf stat -a --` in
  watts to 0.1 W); several sources on one launch; `-r N` with mean, sample
  deviation and perf's relative error (0.018 % from `perf stat -r 5`); exit
  1's last line one of three fixed sentences; `--pid` ending when its
  process exits; the attach moment said; `lifetime.exit_code`/`signal`; the
  launcher's `PATH` search in the parent and the closing round before the
  reap, so `cpu.oncpu_time` on `fs:proc` keeps its last interval. Two
  shared-layer additions (`run_core`'s `bound`, `Session::launch`).
- **emit** (`w24-emit.md`): per-backend `emits:` in the loader, a backend not
  producing an asked emit passed over before the run; `summary.total` where
  the values add up; `summary@<attr>` and `hist@<attr>`; a zero ranking row
  and no `-0.0`; a regrouped poll folded per tick (additive sources only, the
  review's fix); the derived `func` key; `@<attr>` emits over a field the
  backend never fills refused before the run.
- **cpu** (`w24-cpu.md`): the poll's leading event gone (sysstat 2);
  `cpu.pressure` speaking the PSI reader's names; `cpu.branches`; task-clock
  as `cpu.oncpu_time -m count` on `pmu:sw`; the per-state split in `fields`
  (against `mpstat` within 0.5 points); every counting source's `sample`
  served by one sampler on its own counter (`cpu.cycles -m sample -e stack`
  is `perf record`'s default; shares within 0.02 of perf's); `pid` and
  `comm` on every `cpu.profile` sample.
- **gpu** (`w24-gpu.md`): `gpu.mem` following the launched tree (412 MiB
  under a wrapper, 0 before); NVML's held reads said, each event's
  `ts_err_ns` its read's span; the end-of-run clock state read after the
  window (a temperature poll 22-27 % of its program before, 0.75-2.15 %
  after); `gpu.mem_freq`, `gpu.throttle`, the launched tree's engine share.
- **io** (`w24-io.md`): every io program follows forked children (fio's
  forked job 21 329 requests, its own `total_ios`; 0 before); `dir` on
  `io.latency`, `io.queue`, `io.size`; `io.util` (49.37 % against iostat's
  49.38 %); `io.vfs` (100 003 reads and writes against `strace -c`'s); the
  fentry unlink moved after the window (`io.fsync` 265-343 % to 4.7-6.4 %);
  the two caveats.
- **mem** (`w24-mem.md`): `mem.latency`'s delivered rate said, its cause
  shown with `perf record` alone (the IBS unit with `omn`'s two format bits
  settles at 66-74 % of the rate), `peer` checked against perf's decoding;
  `mem.system` (`free -b` to the byte); `mem.oom`; `mem.numa` on `fs:sys`;
  per-process swap-ins through a counting kprobe; VIRT, SHR, Swap,
  AnonHugePages, Locked; the heap's peak, live blocks and lifetimes;
  `mem.sharing`'s per-line detail.
- **net** (`w24-net.md`): the polled baselines at `start()` and no read at a
  poll's `stop()`; `dir` on `net.packets`; `direction`, `saddr`, `sport` on
  `net.capture`; `mcast`, `missed`; `tcp_info`'s other fields (equal to `ss
  -tin`); `net.snmp` (exact against `nstat`), `net.sockets` (the same 29 as
  `ss -tuan`), `net.tcp.lifetime` per launch by its owner, `net.tcp.accept`,
  the kernel stack on `net.drops` (the same 21 frames as bpftrace).
- **power** (`w24-power.md`): `power.freq`'s first poll a baseline; APERF,
  MPERF and the TSC through the `msr` perf unit as `power.freq`'s first
  backend, turbostat's arithmetic exactly; an idle cpu's setpoint said for
  what it is; the core energy domain behind a `domain` key, unkeyed the
  package alone; `power.wakeups`.
- **proc** (`w24-proc.md`): `proc.syscall` counting each call under the name
  it was made with (1000 for 1 before), `errors`, and `syscall`/`comm` keys
  (`strace -c` in one run); `proc.tasks`, the machine's process table that
  `omn top` reads; `proc.signals`' sender; `proc.exec` per launch;
  `proc.calls`, one record per syscall (648 opens, 406 `ENOENT`, as strace).
- **sched** (`w24-sched.md`): the `kind` filter of `sched.irq` (bpftrace 2)
  and the counted switch rate (sysstat 1, 940/s for about 945/s); the
  run-queue summary over `wake_to_run_ns`; `sched.interrupts`,
  `sched.runqueue`, `sched.slices`; `ppid` on `sched.offcpu`,
  `sched.runqlat`, `sched.futex`; `sched.lock` by `kind`.
- **reference** (`w24-reference.md`): the runner's root check by command,
  `teardown:`, the launched program alone, a missing path "does not exist
  here", exit 1 read by omn's last line; `top-cpus`'s bias traced to the
  leading event (fixed by cpu) and its parser refusing the read rather than
  leaving it out; `fio-clat-p50` and `iostat-w-per-s`; gpu's four captures
  and eleven bpftrace, two perfsched, memleak and power captures replaced by
  real output; a tool's own reported losses refused.
- **views** (`w24-views.md`): every view takes the terminal's width and
  height, held at 80x24, 160x48 and 240x60; `--against` and focus in the
  standard set; the usage gradient measured (it fails 3:1 on a light
  terminal, a ramp proposed); the request view's bytes; flows' socket ->
  interface edge; the flame charging own cost as the triage does;
  `Update::Described` and `Key::Dir` in the mirror.
- **top** (`w24-top.md`): `system` lists the machine's processes from
  `proc.tasks` (`top -b`'s `%CPU` and RES to the KiB), splits disk and
  network by `dir`; `process` fills the terminal and reads the shared
  palette, ledger and flame; the batch output's library half
  (`views::system::batch`). **Found**: a live `omn top` of the machine
  starves its pollers.
- **lazy** (`w24-lazy.md`): a family name where a source is expected, a
  `default:` per family in its own catalog file, `--cost low|med|high`,
  every default run here against an independent count; `docs/lazy.md`.
- **unwind** (`w24-unwind.md`): a copied stack sampled after the target's
  `execve` walked by the image it runs now (110 of 41 246 stacks cut before,
  13 after); `Unwinder::symbolizer()`, `perf_map_file()`, `executable()`;
  the share tests' aliasing removed by a pseudo-random pad (worst branch 22.9
  points off its share before, 3.9 after).
- **triage** (`w24-triage.md`): the failure channel; `--program-stderr`
  with `--runs` keeping `FILE.<n>`; the perf-map names from the results'
  `named_by`; the job restricted to the followed child's parent once events
  carry `ppid`; `http-server-stdlib`'s slow client on the MCP way, its two
  open findings removed, and `sqlite3-durability`'s commits both ways.

**At the merge** (`docs/open-findings.md`, "The twenty-fourth wave,
coordinator, after the merge"): `make`, then `make check`, red in the lib
tests (1694 passed, 2 failed, each in the flaky tally and passing alone);
then `--no-fail-fast`: **3048 passed, 13 failed, 15 ignored**. Three were
merge regressions where streams met, all in documents and data, fixed on
`main`: `CATALOG.md`'s `mem.numa` and `mem.swap` rows (`a2ce567`), the view
README's needs table and the system workload's session after `system` asked
`proc.tasks` and keyed by `dir` (`6f3c25a`), `pidstat-w`'s `known_defect`
(`367ad20`). The second `make check`: red on one test in the flaky tally
(`cross_io`, passing alone), 1869 passed. Filed: the portability cells swept
before the wave's sources existed (not a code fault, the matrix sweep's),
`cpu.cycles.system`, and `perf-trace-count`; and with them the wave's one
regression in cost, **`omn trace`'s startup doubled** (37 to 86 ms of user
cpu in the debug build), 45 % of it `emit::values_add_up` parsing the
embedded catalog a second time. The decisions are in `DECISIONS.md`,
"Settled in the twenty-fourth wave".

**After the twenty-fourth wave, in order:**

1. **What the twenty-fourth wave left of the comparison**, by the file each
   lands in: the handoffs' "Not done" items and the requests one stream made
   of another (`docs/fixes/w24-<stream>.md`; the requests are also in
   `docs/open-findings.md`, "Needs from another stream -- the twenty-fourth
   wave"). A **defect** breaks a standing invariant or a number; the rest
   are features.
   - **The shared layers, the coordinator's**:
     - `src/emit.rs` and the catalog loader: **`omn trace`'s startup
       doubled** (37 to 86 ms of user cpu in the debug build), 45 % of it
       `emit::values_add_up` parsing the embedded catalog a second time;
       give it the run's catalog, or parse the embedded one once per process
       (the coordinator's run). Then `cpu.cycles.system`'s band, which needs
       a slack that does not depend on the machine's idle rate.
     - `src/collect/mod.rs` and `omn top`'s session in `src/main.rs`: **a
       live `omn top` of the machine starves its pollers** (top, finding 1:
       `cpu.util` 2 events for about 64 asked, `proc.tasks` one poll, while
       the per-target trace sources watch the whole machine); a poll round
       that never runs is counted nowhere. Rerun alone first ("Still to
       run", item 13).
     - `src/bpf/fentry.rs`: `Narrowing` with a descendants map, so
       `io.syscall`, `io.fs` and `net.socket` follow forked children (io;
       lazy's io default names `io.syscall`, so `omn io -- ./script.sh`
       reads as an idle program until it lands).
     - `src/perf.rs`: `PERF_SAMPLE_PERIOD` decoded, so the frequency-mode
       samples of `cpu.instructions`, `cpu.branches` and the misses carry a
       `weight` and their shares are perf's (cpu).
     - `src/catalog.rs`: a rate unit (`bytes_per_second`) for `gpu.pcie`
       (gpu); an entry-level `stacks: device` on `gpu.profile`, so
       `DEVICE_STACKS` goes and `check_stack` reads the entry (unwind, gpu,
       emit).
     - `src/bpf/mod.rs`: a `mod` line for a per-cpu `sched.runqlen` program
       on a 99 Hz cpu-clock event (sched; `sched.runqueue` is the machine's
       sum meanwhile).
   - **`src/request.rs`, `src/event.rs`, `src/filter.rs`, `src/format.rs`
     (emit)**: keys asked of the seam -- `op` (`io.vfs`, and `syscall`'s
     general spelling), a counter name (`net.snmp` as series), `level`
     (`mem.latency`), `mapping` and `user` (the memory sources, top 4 and
     8); a request naming its value attribute (`registers`, `memory_hz`,
     `process_ratio`, `busy`, `avg_hz`), and a summary that also sums a named
     attribute per series (`proc.syscall`'s `errors`, so `-k syscall -e
     summary` is `strace -c` with its errors column); timeline and rate
     counting an event without the value as unmeasured, as summary does
     (`power.freq` through `pmu` on a cpu that never left idle); `filter.rs`
     reading an attribute named `kind` before the event's own kind (sched's
     root cause of the irq defect, worked around in the collectors); an
     `object` key (it needs the unwinder to keep every frame's object) and
     an inclusive per-function share (perfrecord 3-4); a `pcap` encoder
     (net); `omn list --format json` carrying the defaults and each
     backend's own `emits` (lazy, emit).
   - **The catalog lines one stream asked of another** (`catalog/*.yaml`):
     - **heap 1, still a defect on `main`**: stream emit's resolver refuses
       an emit a backend does not produce before the run, but
       `catalog/mem.yaml`'s `preload` backend of `mem.heap.alloc` does not
       yet say `emits: [hist, topk, raw, summary]`, so `omn trace
       mem.heap.alloc -e stack -- prog` still runs the program and refuses
       after it. One line (emit's request to mem).
     - `func` in `cpu.profile`'s `keys:` (and the counting sources that
       sample), now that `KeyKind::Func` is on `main`: one line, perfrecord
       3 (emit's request to cpu).
     - `cpu.oncpu_time`'s `fs:proc` caveat, "a launched program is reaped
       before the run's closing read", no longer true (run).
     - `mem.cache_l1`, `mem.cache_llc`, `mem.cache_tlb`' `sample`, a row
       each in `profile::SAMPLED` (cpu, for mem's entries).
     - The lazy defaults: `net.tcp.lifetime` in net's `med` now that it
       serves a launch; `proc.tasks` beside `proc.threads` in proc's `low`
       (net, proc).
     - `catalog/references/perfrecord.yaml`'s `load-latency` `why`: the
       cause of the IBS rate and `peer` (mem).
   - **`src/collect/cpu/`**: `cpu.profile`'s provenance filling `perf_maps`
     and `executables` from the unwinders (`Unwinder::perf_map_file`,
     `executable`; unwind gives the lines); `start()` priming
     `/proc/kallsyms` before the window, which is why `cpu.profile --pid`
     attaches 1.0 to 1.5 s after omn starts (run, with unwind's
     `src/stack.rs`); `cpu.context_switches -m count -e rate` dividing by
     `enabled_ns`, the defect sched fixed for `sched.switches` (sched);
     instruction attribution, an `insn:<func>+0x<off>` key (perfrecord 7);
     `cpu.profile`'s `pmu` row, removed or kept as a pointer to `cpu.cycles
     -m sample` (the coordinator's call, `DECISIONS.md`).
   - **`src/collect/sched/`**: `named_by` and `perf_maps` beside
     `sched.offcpu`'s stacks, so the flame and the triage charge off-cpu own
     cost alike (views; carried from the twenty-first wave); `ppid` as a
     filter field and on `sched.switches`, `sched.migrations`,
     `sched.wakeup`, `sched.lock`; `sched.lock`'s caller and address
     (perfsched 6); the tracepoints' wakeup-to-switch wait in
     `sched.slices`.
   - **`src/collect/io/`**: a block request a `kworker` dispatched, charged
     to the bio's submitter (carried; not reproduced on a launched program,
     seen machine-wide); `io.bytes` on `fs:proc` counting bytes from before
     the window in its first event, up to 3.4 s of startup (top, finding 2).
   - **`src/collect/net/`**: a `bpf:raw_tp` road for `net.tcp.connect`,
     whose 69 ms in-window cost is its six trampolines' detach; the captured
     header bytes for pcap; `net.tcp.lifetime --tid`; `net.tcp.accept`
     following a launched tree; `ss -s`.
   - **`src/collect/mem/`**: the heap's eBPF attach (heap 2, a stream of its
     own); `mem.oom`'s victim (`oom:mark_victim`); a per-cache slab
     (`/proc/slabinfo` is root's).
   - **`src/collect/power/`**: `power.cstate -e rate` (a counted window);
     `power.wakeups --system` through `proc.tasks`; the per-process energy
     estimate.
   - **`src/collect/gpu/`, `shim/`, `cupti/`**: `gpu.pcie` (measured, waits
     on the unit); launch facts as values; NVTX, `cuMemsetD8`,
     `cuModuleLoadData`, `cuCtxCreate`, a callback backend (the shim's and
     cupti's owners); `gpu.freq`'s three clock calls a reading; whether the
     GPU bench counts a driver-held late tick as a lost record (the bench's
     owner).
   - **`src/collect/proc/`**: `proc.calls` on `bpf:tp`, and paths for
     `open`, `openat2`, `execve`; `proc.exec`'s tree exact through
     `sched_process_fork`.
   - **`src/main.rs` and `src/cli.rs` (run)**: `omn top --batch [--sort
     cpu|memory|faults|io|runqueue]`, the flag over the landed
     `views::system::batch` (top); `omn latency` and `omn saturate` exiting
     1 when their answer carries `failures` (triage); `Tap::described`
     sending `Update::Described` on every change (views); `Beside::capacity`
     filled once the model has it (run, views, top); the process cpu spent
     after a window closed said in the cost line (io); the `--emit` help
     naming `summary@<attr>` and `hist@<attr>`, and `--program-stderr`'s
     `FILE.<n>` (emit, triage).
   - **`view/`**: `Beside::capacity` and the `Lifetime` mirror's `exit_code`
     and `signal` in `view/src/model.rs`, `Capacity::of` reading it for a
     replay (run, top); flows' machine band folding `dir:` series into one
     sink per device, as `system::fold_dirs` does (top); `usage::RAMP`
     under 3:1 on a light terminal, `28, 64, 100, 136, 166, 160` proposed,
     then the ignored test un-ignored (views to top); `ON LOCKS -58 lost`
     reading as a negative count; the `process` suspect reading sleep where
     `sched.futex` holds the wait (the timebar subtracting the futex waits);
     flows' file -> device ribbon; a placed drop on screen.
   - **`tests/reference/` and `catalog/references/`**: `perf-trace-count`
     skipping, not failing, when perf says it lost events (the runner's
     owner); pairs for what landed -- `io.util` against `iostat %util`,
     `io.vfs` against vfsstat or `strace -c`, a forked fio; `net.snmp`
     against `nstat`, `net.sockets` against `ss -tuan`, `ss -ti`'s other
     fields, tcpaccept, tcplife per launch, the drop stacks,
     `tcpdump-captured` through `direction != "outgoing"`;
     `sched.interrupts` against vmstat's `in`, `sched.runqueue` against
     `r`/`b`, `sched.slices` against `perf sched timehist`, runqlat's mean
     against `wake_to_run_mean_ns`, `perf lock contention`'s type against
     `topk@kind`; `Busy%`/`Avg_MHz` against turbostat, `energy-core`
     against `perf stat -a`, `pidstat -w` against `power.wakeups`;
     `cpu.util`'s `summary.mean` against `mpstat`; `omn top --batch`
     against `top -b`; the parsers reading `summary.total`; the pairs
     written as `omn ... --system -- workload` in place of the tool wrapping
     omn (perfstat 3, power 1, io 3, top 6, net); a `mem.slab` pair once
     slabtop is granted.
   - **The cases** (`tests/cases.rs`, `src/triage/`): `make-sqlite`'s
     critical path rerun now that `ppid` is on the sched sources (its
     open finding should then fail as passing); `read_mcp`'s job by `ppid`;
     `sqlite3-durability`'s `every-commit-waits-for-the-disk` (blocked 24
     to 27 ms over `schedstat` where 8 and 12 are allowed).
   - **Documents, the coordinator's**: `docs/triage.md` (the answers'
     `failures` and `program_stderr`, `followed.job.under`, `unplaced`,
     `unplaced_on_cpu`), `docs/cases.md` (the agent's reading of a thread
     blocked on a connection, the eBPF samples set aside, `named_by` first),
     `docs/unwind.md` (the exec re-read, `CALLTREE`'s pad),
     `docs/view/system.md`, `docs/view/process.md` and
     `docs/view/request.md` ("bytes on the network" is stale), `GUIDE.md`
     (the run's new shape), `docs/bench.md`'s `io.fsync` row (the share was
     the unlink, now after the window; `net.tcp.connect`'s 39 % is likely
     the same).
2. **What the user grants.** Every reference pair that wanted root now runs
   through `sudo -n` (`/etc/sudoers.d/omniscient-witnesses`, cpupower
   included), and every capture written from documentation has been
   replaced by the tool's real output but power's wrapped one. What is left:
   `make CAPTURE=1` before the reference run (`tcpdump-captured` skips
   without it), the boot-time settings, and `slabtop` in the sudoers line
   for a future `mem.slab` pair (`slabtop -o -s c` reads `/proc/slabinfo`,
   root's):

   ```console
   sudo mount -o remount,mode=755 /sys/kernel/tracing          # after each boot
   sudo chmod o+r /sys/class/powercap/intel-rapl:0/energy_uj   # after each boot
   cd /home/moha/repos/omniscient && make CAPTURE=1
   cargo test --test reference -- --test-threads=1 --nocapture
   ```

   And the runs "Needs privilege" carries.
3. **The views' open points left**, after the user's look at a terminal: the
   mouse in Ghostty and through tmux, and the gradient by eye on a light
   terminal (item 9 of "Needs privilege", and the two `gradient--*.ansi`
   strips); the ramp itself, the capacity in a recording, `Update::Described`
   sent, flows' `dir` fold, the file -> device ribbon and a placed drop
   (item 1, `view/` and `src/main.rs`). Landed in the twenty-fourth wave:
   every view fills the terminal, `system` lists the machine's processes
   and splits by `dir`, `process` on the shared palette, the request view's
   bytes, the socket -> interface edge, `--against` and focus snapshots,
   `sqlite-latency`'s `blocked` with its events, a recording holding a drop.
4. **The lazy mode, what is left**: `omn list --format json` and `omn-mcp`
   offering the defaults (a `family` + `cost` argument through
   `omn::lazy::compose`); a low band this machine cannot keep is said, not
   avoided -- `cpu.profile` (`pmu:sw`), `mem.rss` (`smaps_rollup`, with
   `statm` free below it) and both gpu sources declare `med` at `--cost
   low`, and choosing the backend by band is the resolver's change; and the
   io default's children (item 1, `src/bpf/fentry.rs`). Landed: the
   spelling, a default for every family or its reason, the loader's
   refusals, every default checked here against an independent count.
5. **Then, once `omn` is polished: a portability wave across
   distributions** -- Debian, Fedora, Ubuntu, Gentoo, NixOS; x86_64 and
   aarch64; containers -- each measured, never read from documentation: what
   `omn list` serves there, which backends attach, what each refusal says,
   and the reference pairs run on each where its tools install.

Below the milestones, and not scheduled:

- **CTF import** — ingest an LTTng-UST stream and merge it into the timeline.
  A consumer of one more format, not a source: `DECISIONS.md` boundary 6 says
  why LTTng is a peer rather than a backend. Low priority; nothing depends on
  it and no measurement here is worse without it.
- **Five decisions are open** (`DECISIONS.md`): the phase 6 criterion for
  the in-kernel histogram, pending what `tp_btf` leaves; what the budget's
  first check may stop a run on (its one-time half landed in the twentieth
  wave, the 10 ms slack proposed with numbers); a cost per regime in the
  catalog's schema (proposed with numbers); and since the twentieth wave,
  whether the latency session asks
  `sched.wakeup`; and since the twenty-fourth wave, whether `cpu.profile`'s
  `pmu` row stays as a pointer to `cpu.cycles -m sample` or goes. How a reference pair reads a value `omn` computes was
  settled before the twenty-third wave: a parser of `omn`'s output. How a byte counter's direction becomes a
  series was settled in the twenty-first wave: a key, `dir`.
- **An `lbr` collector** waits on a cpu with a branch call stack (Intel from
  Haswell) to be tested against, and **`sframe`** on a kernel that reads
  `.sframe` (none through 7.2).
- **A preempted eBPF program skips every run behind it on that cpu**
  (`docs/bpf-layer.md`, "Losses"): counted, never silent, but a program that
  fires on every page-cache lookup loses the target's events to the machine's
  load. A per-target attach would narrow it.

The fifth wave closed 52 of the sixty-three defects the scenario wave found;
the eleven that are left are named in `docs/fixes/`.

### 1. ~~The catalog pass, the collectors, and the short distribution~~ — **done**

The sixth wave landed twenty-two of its twenty-four sources and closed finding
37. `docs/families/*.md` is the per-stream record and `docs/briefs/` keeps the
brief. What it moved:

- **`scoped_eq` has a producer**, which was the point. `io.latency` and
  `io.queue` join per request at 99.9 % attribution, and the key is visibly
  `(pointer, open_ts)` — the same pointer recurs under two timestamps in one
  five-second run, which is the recycling the scoped key exists to catch.
- **`proc` and `power` are complete.** `sched` is one short, `net` three, `mem`
  three.
- **A distribution says when it is short.** `emit::Shortfall` rides on
  `SeriesHist` and `SeriesSummary`, so every format carries it at once, and a
  run that lost a tenth of its events marks every count, mean and maximum as a
  lower bound rather than a measurement.

### 2. What is left: three sources, and none of them is this machine's to serve

The ninth wave took six of the eleven -- the page cache, the preload shim
pointed at `pthread_mutex_lock` and the GL/CUDA entry points, and AMD IBS --
the nineteenth took the two CUPTI waited on, and what the last three wait on
is not a collector nobody has written:

| Waits for | Sources |
|---|---|
| uncore PMUs | `mem.bandwidth` |
| Intel PT, and this machine is AMD | `cpu.flow` |
| an XDP program, the one type here that can drop packets rather than observe them | `net.xdp` |
| ~~CUPTI, Nsight~~ | ~~`gpu.kernel`, `gpu.counters`~~ -- served through CUPTI since the nineteenth wave, with `gpu.profile` |

**Breadth is no longer where the project's remaining work is.** What is left is
depth: the vertical case's two network rows, hot attach, the self-benchmark,
and the layer above the engine.

### 2b. The eleven defects the fifth wave left

`docs/fixes/` names each with why it was left: two need a line in a file the
stream did not own, three are decisions rather than tasks, and one — `topk`
ranking a series rather than an attribute — needs surface syntax nobody owns.

### 3. ~~The crossings~~ — **landed by the seventh wave**

Four questions a single family can't answer, each now a session, a test and a
document: `docs/scenarios/cross-memory.md`, `cross-compute.md`, `cross-io.md`
and `cross-machine.md`. Twenty-seven scenarios, eleven sessions, and **no file
under `src/` changed** — which was the point: every source they join already
collected and was already checked, and what was new was the arithmetic that puts
two families in one table.

What they produced beyond the tables is a list of things the engine cannot yet
say, each filed with the signature wanted:

- **The evidence ladder has a gap.** Three of the four streams independently
  asked for a match kind that is `TEMPORAL` by construction: today the only
  rules that validate between two families' events report `KEY`, which
  overstates what crediting-by-recency actually knows.
- **A decomposition is not an emit.** The compute crossing computes its three
  states and its residue in the test rather than in the tool, because nothing in
  `src/` renders a decomposition beside the joins that produced it.
- **The I/O path starts at the block layer and says so.** The boundaries above
  it need `fentry`.
- **`mem.reclaim` stamps its windows at the close**, so a join opening on it
  misses the first window — found by the machine crossing, filed for `mem`.

The seventh wave's shared-layer stream is `docs/families/shared-layer.md`: the
sixth wave's twenty requests landed once, which is what unblocked two `io`
sources and put the capability advice where the failure actually is.

### 5. The smaller ones, each with its reason

- ~~**`Unit::Watts` does not exist**~~ — it does since the fourteenth wave, and
  `gpu.power` counts a window's energy and its rate is the mean draw in watts.
  The text still prints that rate as `J/s` (filed for R).
- ~~**The GPU is not in the environment capture**~~ — it is, and has been since
  `f5ae1c6`. The resolved file carries every card the kernel describes, by PCI
  ids, driver and driver version, read from sysfs in 47 µs rather than from
  NVML: `{"card": "card1", "pci_id": "0x10de:0x1e07", "driver": "nvidia",
  "driver_version": "615.71.09"}`. What it deliberately does not carry is in
  `not_captured` beside it, vendor library versions included. This line stayed
  in two documents after the thing it describes was done, which is its own small
  lesson about where a claim lives.
- **`net.capture` needs `CAP_NET_RAW`**, which `make` grants only when asked,
  as `make CAPTURE=1`. Settled in `DECISIONS.md` in the tenth wave: it is the one
  capability here that acts rather than observes.
- **The closing read of a polled counter** was settled in `DECISIONS.md` ("the
  runner closes the window"): `io` and `power` are right, and `net/dev.rs` still
  reads again in `stop`. The fix is one function in a file stream N owns this
  wave, and is filed with it.

---

## Needs privilege

`/etc/sudoers.d/omniscient-privileged` grants without a password the exact
commands below (`sysctl kernel.perf_event_paranoid` 2 and -1,
`kernel.bpf_stats_enabled` 1 and 0, the two `null_blk` loads and `rmmod`,
`chmod`/`chown` of `/dev/nullb0`, `tc netem` on lo, a nosuid tmpfs) and
`dmsetup`/`losetup`, beside `setcap` on `target/debug/omn` and
`target/release/omn` (`sudo -n -l` lists them). Each of these is
machine-wide, so **the coordinator runs them alone**, never while streams
build or test.

### Still to run, after the twenty-fourth wave

**Added by the twenty-fourth wave**: items 11 to 14 below, and item 10 done
(every reference pair that wanted root ran through `sudo -n` in the wave and
at its merge; no pair skips on it). The streams ran only what `make`'s
capabilities and the granted witnesses allow; nothing machine-wide, and no
bench. Item 1 is now wanted for a reason of its own:
`portability::every_cell_covers_the_whole_catalog` stays red until the cells
are swept with the wave's sixteen new sources.

#### Carried from the twenty-third wave

**Items 2 to 5 ran on 2026-10-05**, alone after the reboot onto the NVIDIA
driver (`docs/open-findings.md`, "The runs carried since the nineteenth
wave"): GPU tests, cases and views green; the GPU bench red on `gpu.freq`'s
two lost records; the backends bench green with 16 bands outside, to move.
Items 6 to 8 ran the same night, typed by the user (below, and
`docs/open-findings.md`); item 9's one remark is the views' size; item 10 is
the reference pairs, done after the reboot.

Item 1, the matrix, ran after the twentieth wave's merge (2026-10-04, "What
the earlier runs found" below). In the twenty-first to twenty-third waves
the streams ran only what `make`'s capabilities allow, and nothing
machine-wide; the reference tools that need root are item 10. **Carried,
none of it run in the twenty-second or twenty-third wave**: the backends bench (since the nineteenth wave), the GPU bench and
the GPU tests (`gpu.profile` measurable since its double free was fixed;
the failure channel's exit 1, and the bench's new schedule), the budget's
two runs, and the net stream's namespace run. Items 6, 7 and 10 need a
password (`sudo -n` refuses them); item 9 is the user's. The GPU's
reference pairs (about 70 s, the device to themselves) belong with item 2.

In the order that changes the machine least between them. The GPU is one
device: every GPU run below wants it to itself, so check first that nothing
holds it (`nvidia-smi --query-compute-apps=pid,process_name --format=csv`
prints only its header), and run nothing that polls NVML beside it -- a
poller makes the energy counter under-count for every reader of the board.

1. ~~**The matrix**~~ -- **ran after the twentieth wave's merge**, every
   judge green and `make check` green after it (below). Kept for the next
   sweep (alone, about 30 minutes for the twenty guests here, plus
   `scripts/matrix.sh here` for `here-caps` and the `here-nocaps` recipe in
   `docs/portability.md`, "The twentieth wave's sweep"):

   ```console
   cd /home/moha/repos/omniscient && make matrix
   cargo test --test portability
   make check
   ```

   Read the judges per cell, `every_cell_covers_the_whole_catalog` green, the
   sched fallbacks on the kernels before 6.16, `cpu.oncpu_time` falling back
   to `fs:proc` on a kernel before 6.8 (where `sched_stat_runtime` has three
   arguments), and the `member_offset` audit on each kernel (`net_qdisc`'s
   `possible_net_t` among them).

2. **The GPU tests on a device nobody else uses** (no root):

   ```console
   cd /home/moha/repos/omniscient && make
   nvidia-smi --query-compute-apps=pid,process_name --format=csv
   cargo test --test gpu_counters --test gpu_kernel -- --test-threads=1 --nocapture
   cargo test --test family_gpu --test scenario_gpu
   ```

   Read: `a_native_program_exits_on_its_own_under_every_inside_source` green
   (the double free's test), and the counters-beside-kernel refusal run, not
   skipped. And, if a CUDA program killed in the injection library's exit
   handler reproduces (the twenty-first wave, stream run):

   ```console
   target/debug/omn gpu.counters -- <cuda program>; echo "exit $?"
   ```

   expected: the `degraded:` line as before, then `error: pid N died inside
   omn's injection library ...` and exit 1.

3. **The bench, GPU and backends** (alone on the machine and the device, the
   release build and its capabilities; for `gpu`, **about fifteen minutes
   reasoned** since a replaying arm runs 2000 launches (the twenty-first wave,
   stream docs; two hours before), not yet measured; 68 minutes for
   `backends`):

   ```console
   cd /home/moha/repos/omniscient && nvidia-smi --query-compute-apps=pid,process_name --format=csv
   scripts/bench.sh gpu
   scripts/bench.sh backends
   ```

   Read the GPU step's wall time and the `gpu.counters` rows against their
   scaled baselines (`scaled_series` in `table.json`). Then
   `target/bench/gpu-<stamp>/record.md` over the `gpu-bands` block of
   `docs/bench.md`, `gpu.profile`'s `pending` row replaced by its measured
   one and `gpu.temp`'s band moved to `med` if it reads over 1 % again, and
   `cargo test --test bench_gpu`; the backends table into its record,
   `cargo test --test bench_bands` naming every band outside -- the
   eighteenth wave's six, `sched.wakeup`'s `med` now that four programs run on
   every interrupt of the machine, and `cpu.oncpu_time` through `bpf:raw_tp`'s
   `low`.

4. **The budget's meters, and its first check at paranoid 2** (stream budget;
   `sudo -n`, alone):

   ```console
   cd /home/moha/repos/omniscient && make
   sudo -n /usr/bin/sysctl kernel.bpf_stats_enabled=1
   cargo test --test budget what_an_ebpf_program_spends_inside_the_target -- --nocapture
   target/debug/omn cpu.oncpu_time --system -m poll@100ms -e summary -d 10s
   sudo -n /usr/bin/sysctl kernel.bpf_stats_enabled=0
   sudo -n /usr/bin/sysctl kernel.perf_event_paranoid=2
   target/debug/omn trace cpu.profile -e stack --stack fp --symbols debuginfod --budget 20% -d 3s -- python3 -c 'import time
   for i in range(40):
       s=0
       for j in range(20000): s+=j
       time.sleep(0.02)'
   sudo -n /usr/bin/sysctl kernel.perf_event_paranoid=-1
   ```

   Read: `proc.syscall`'s programs, dropped in its `stop`, read before it
   (`in_target_runs >= 2 * ops` with no budget); the in-target time of
   `cpu.oncpu_time`'s program in its cost line; and the first check of a
   profile whose kallsyms reads as zeroes (116 to 179 % before the wave), with
   the one-time load said apart.

5. **Alone, no privilege**: the cases and the view workloads, which measure
   the machine's load.

   ```console
   cd /home/moha/repos/omniscient && make
   cargo test --test cases -- --nocapture
   cargo test --test view_workloads -- --nocapture
   ```

   Read: whether `http-server-stdlib`'s slow client and `make-sqlite`'s and
   `gcc-shell`'s job findings now pass (the test refuses a listed finding that
   passes, which says which to drop). **Ran in the twenty-first wave**, with
   other streams on the machine: the cases twice (docs; `gcc-shell`'s finding
   dropped, the others still listed) and `view_workloads` 14 of 14 (record).
   Worth once more alone, after item 3.

   And on a machine whose tracefs is readable, the exit the proc documents
   now qualify (the twenty-first wave, stream docs):

   ```console
   cd /home/moha/repos/omniscient && make && target/debug/omn -f examples/proc-not-built.yml; echo $?   # 3
   ```

6. **A socket in another namespace** (stream net; `ip netns` is **not in the
   sudoers file**, so typed by the user):

   ```console
   sudo ip netns add omn-w20 && sudo ip netns exec omn-w20 ip link set lo up
   # a TCP pair inside omn-w20, while from the host:
   /home/moha/repos/omniscient/target/debug/omn net.socket --system --mode trace --emit raw --duration 2s --format json
   sudo ip netns del omn-w20
   ```

   Expected: that pair's rows carry no `iface`; this namespace's still `lo`.

7. **The device's clocks, set and put back** (device; root, **not in the
   sudoers file**, so typed by the user; each alone on the board):

   ```console
   # does a -lgc lock show in the performance levels, and does omn say "locked"?
   sudo nvidia-smi -lgc 1500,1500
   /home/moha/repos/omniscient/target/debug/omn gpu.temp --mode snapshot --emit summary -- true   # "shader clock locked at 1500 MHz"?
   python3 -c 'import ctypes as C;n=C.CDLL("libnvidia-ml.so.1");n.nvmlInit_v2();d=C.c_void_p();n.nvmlDeviceGetHandleByIndex_v2(0,C.byref(d))
   class S(C.Structure):_fields_=[("v",C.c_uint),("s",C.c_char*2048)]
   s=S();s.v=C.sizeof(S)|(1<<24);print(n.nvmlDeviceGetPerformanceModes(d,C.byref(s)),s.s)'
   sudo nvidia-smi -rgc
   # persistence mode read back as on
   sudo nvidia-smi -pm 1
   /home/moha/repos/omniscient/target/debug/omn gpu.temp --mode snapshot --emit summary -- true   # "persistence mode on"?
   sudo nvidia-smi -pm 0
   ```

   If the first prints "not shown locked", the levels do not show a `-lgc`
   lock and the verdict needs another source of evidence (filed against
   `src/collect/gpu/mod.rs`, `shader_lock`); if "locked at 1500 MHz", a
   range may read as free again. Either way it closes the open line in
   `DECISIONS.md`.

8. **From the eighteenth wave, still not run**: the known-latency device at
   3 ms, once its line is in `/etc/sudoers.d/omniscient-privileged`
   (`/usr/bin/modprobe null_blk queue_mode=2 irqmode=2 completion_nsec=3000000 nr_devices=1 gb=1 bs=4096`),
   with the test's margin first widened by the timer's slack
   (`docs/open-findings.md`, the eighteenth wave after the merge):

   ```console
   sudo -n /usr/bin/modprobe null_blk queue_mode=2 irqmode=2 completion_nsec=3000000 nr_devices=1 gb=1 bs=4096
   udevadm settle --timeout=10
   sudo -n /usr/bin/chmod a+r /dev/nullb0
   cargo test --test scenario_io -- --ignored --exact \
       a_device_with_a_known_latency_puts_every_request_in_its_bucket_in_both_histograms --nocapture
   sudo -n /usr/bin/rmmod null_blk
   ```

   And, optional: `http-server-stdlib`'s `a-cold-file` on a `dm-delay`
   device of 50 ms reads (`docs/fixes/w17-python.md` section 6; `mkfs.ext4`,
   `mount` and `chown` are not in the sudoers file); the shim's saving on a
   `malloc`/`free` loop under a `sched.mutex`-only session, before and after
   `0a25793`, with the release shim.

9. **Not privilege: the user at a terminal** -- the mouse, the links, the
   `process` view as the twenty-first wave redrew it and the usage gradient
   on a dark and a light terminal and in 256-colour tmux, which no stream
   could see (`docs/view/gallery/README.md` has a row per view):

   ```console
   cd /home/moha/repos/omniscient && make
   target/debug/omn top --replay tests/view_fixtures/http-server
   #   click a tab, a process row in `system` twice, right click back, the wheel
   #   on the heatmap; M, then select text with the mouse; again inside tmux,
   #   with and without `set -g mouse on`
   target/debug/omn top --replay docs/view/gallery/process-run   # a line twice opens its page, right click back
   target/debug/omn top --gallery tests/view_fixtures/workload-lanes   # space steps the views, 0 is process
   target/debug/omn top -- python3 -c 'import time; [sum(range(10**6)) or time.sleep(0.01) for _ in range(300)]'   # opens on process, CPU in the gradient
   target/debug/omn top --view system                                     # the gradient, disk and network split
   target/debug/omn top --view ledger -- python3 view/workloads/timebar/workload.py   # the blocked line, Enter -> timebar
   ```

10. ~~**The reference pairs that need root**~~ -- **done in the twenty-fourth wave**: stream reference ran every pair through `sudo -n` and replaced the captures (`docs/fixes/w24-reference.md`), and the coordinator's quiet run had no skip on root; only `tcpdump-captured` wants `make CAPTURE=1` first (item 2 of "What remains"). As it was written (the twenty-second wave's, run
    through the shared runner since the twenty-third; typed by the user,
    nothing here is in the sudoers file): the 20 pairs that skip on
    `sudo -n`, and the captures still written from documentation. The
    commands are "What remains", item 2: bpftrace's ten pairs against the
    tools for the first time; perfsched's six `perf` pairs, whose tolerances
    were set from `omn`'s spread alone; memleak's outstanding bytes;
    tcpdump's count through the runner; the RAPL files and cpupower's Mperf.

#### Added by the twenty-fourth wave

11. **The held clocks' share with a lowered power limit** (stream gpu; root,
    **not in the sudoers file**, so typed by the user; the board to itself,
    the limit put back after):

    ```console
    nvidia-smi --query-gpu=power.default_limit --format=csv
    sudo nvidia-smi -pl 150
    cd /home/moha/repos/omniscient && target/debug/omn trace gpu.throttle -m count -e raw --format json -- python3 tests/gpu_workload.py kernel 3000
    sudo nvidia-smi -pl 250     # or the default limit read above
    ```

    with `nvidia-smi dmon -s v` beside it. Read: `power_cap_ratio` above 0
    where `dmon`'s `pviol` is, and `gpu.throttle`'s value the largest
    limiting share. Then `scripts/bench.sh gpu backends` (item 3) reads the
    NVML pollers against their bands now that the end-of-run clock state is
    read after the window, `gpu.freq`'s late ticks as lost records or not,
    and `io.fsync`'s row near 5 % (its reason in `docs/bench.md` then fails
    `bench_bands` by design: the share was the fentry unlink, now after the
    window).

12. **The reference pairs, quiet and with the capture's capability** (no
    root beyond what `sudo -n` grants; alone, nothing on the GPU):

    ```console
    cd /home/moha/repos/omniscient && make CAPTURE=1
    cargo test --test reference -- --test-threads=1 --nocapture
    ```

    Read: `tcpdump-captured` held, not skipped; `perf-trace-count` and
    `cpu.cycles.system` as filed (`docs/open-findings.md`, the twenty-fourth
    wave); `runqlat`, `fio-bytes`, `perf-sched-timehist-sched-in` and
    `psi-cpu-some`, red only under the parallel workspace run, held.

13. **A live `omn top` of the machine, alone** (stream top, finding 1; no
    privilege beyond `make`'s):

    ```console
    cd /home/moha/repos/omniscient && make
    target/debug/omn top --headless -d 8s -o /tmp/omn-top-machine
    python3 -c "import json; d=json.load(open('/tmp/omn-top-machine/results.json')); [print(r['label'], r['events_seen']) for r in d['results']]"
    ```

    Read: `cpu.util@system` near 32 events for 8 s at `poll@250ms`, not 2;
    if it is 2 alone too, the starvation is the runner's, not the load's.

14. **A root `bpftrace` stream net left running** (`sudo -n bpftrace
    /usr/share/bpftrace/tools/tcpaccept.bt`, pid 204407, still running when
    the wave was merged; `sudo -n` does not grant its `kill`). By a person:

    ```console
    pgrep -af tcpaccept.bt && sudo kill -INT 204407
    ```

    And item 5's cases once more alone: `make-sqlite`'s
    `every-cited-number-agrees-with-the-reference` should now close (its
    open finding failing as passing) since `ppid` is on the sched sources.

### What the earlier runs found

**Ran after the twentieth wave's merge** (2026-10-04, at `3a1aaa1`, alone on
the machine; `docs/portability.md`, "The twentieth wave's sweep"):

- **Item 1, the matrix: every judge green.** `make matrix` swept the twenty
  guest cells (4 kernels x `caps`, `root`, `nocaps`, `nobtf`, `notracefs`)
  in 27 minutes, every one complete with 0 broken; `make matrix` itself
  stopped red because it does not sweep `here-*`, which still predated
  `gpu.profile`. `scripts/matrix.sh here` then swept `here-caps`, and
  `here-nocaps` was swept from an unlinked copy of the binary (the script's
  own `make` puts the capabilities back, so the sweep binary was run directly);
  `make` restored them. All 22 cells from one build (`a0fa46876ef690f3`),
  102 sources each. `cargo test --test portability`: **25 passed**, 1 ignored;
  `every_cell_covers_the_whole_catalog` green; every gap between the 22 cells
  explained by a recorded field (`privileges` 2528, `kernel_btf` 1748,
  `tracefs` 1570, `kernel_release` 383, `virtualization` 280, `gpus` 200,
  `perf_event_paranoid` 24), none the build's. Usable per cell class:

  | Kernel | caps | root | notracefs | nobtf | nocaps |
  |---|---|---|---|---|---|
  | 5.15.221 | 74 | 78 | 78 | 48 | 44 |
  | 6.1.188 | 77 | 81 | 81 | 48 | 44 |
  | 6.6.157 | 81 | 82 | 82 | 48 | 44 |
  | 7.2.7 | 83 | 84 | 84 | 48 | 44 |
  | here (7.2.7, no guest) | 95 | | | | 59 |

  Read as item 1 asked: the sched sources serve through `bpf:raw_tp` from 6.1
  up; on 5.15 `sched.runqlat`, `sched.offcpu` and `sched.lock` are refused
  with each reason named (the three-argument `sched_switch`, the floor, and a
  fallback with no collector), as before. `cpu.oncpu_time` falls back to
  `fs:proc` on 5.15 and 6.1, where `sched_stat_runtime` has three arguments,
  and serves through `bpf:raw_tp` on **6.6.157** as on 7.2.7: the two-argument
  prototype is already in that 6.6 stable kernel, so "before 6.8" in
  `docs/fixes/w20-cpu.md` is the mainline boundary, not the one a 6.6 LTS
  meets. `net.qdisc` serves through `bpf:raw_tp` on every kernel with the
  capabilities, so the `member_offset` lookups it makes (`possible_net_t`
  among them) resolved on each.
- **Then one `make check`: green on its first run**, 2660 passed, 0 failed,
  13 ignored across 64 binaries, clippy and fmt clean. Nothing from the flaky
  tally fired, so nothing was rerun.

**Ran after the nineteenth wave's merge** (2026-10-04, at `a71d9cf`, alone on
the machine and the device: `nvidia-smi --query-compute-apps` empty before
each GPU run; `docs/open-findings.md`, "The nineteenth wave, coordinator,
after the merge"). The coordinator stopped the list after the GPU bench: the
matrix, the backends table and the full suite wait for the twentieth wave.

- **Item 2, the GPU tests: green.** `make` (capabilities already on);
  `gpu_kernel` 11 passed. Each self-timed kernel lasts **+0.93 to +1.25 us**
  more than its own span with nothing else on the device (median about
  +1.0 us, three runs of 20 launches by hand, `ts_err_ns` 171 to 204), starts
  6.9 to 39 us after the host's `before` and ends 1.0 to 1.8 us before its
  `after`: the +1.0 to +1.3 us measured with other tenants is CUPTI's own, not
  theirs. `gpu_counters` 8 passed, no `skipped:` line; by hand, 12 of 12
  launches undisturbed on both workloads, the copy's DRAM reads 64.0 to
  64.4 MiB "reproducible here", the spin's "not dominated by this workload",
  instructions bit for bit on both (3 167 488 and 353 620 672). `family_gpu`
  10 and `scenario_gpu` 32 passed. `make install-check` and `make
  dist-verify` passed: 102 sources, the shim's two allocations, `omn-mcp`
  serving the same 59 sources as `omn`, and on the extracted tree
  `libomn_cupti.so`'s glibc floor 2.34 and the tarball's 10 paths.
- **Item 3, `scripts/bench.sh gpu`: measured, its test red on one row**
  (`target/bench/gpu-1791113670/`, 117 minutes, gate 0.62 / 0.56 / 0.10 %).
  `gpu.counters` measured for the first time: **+282 934 %** launch-bound,
  +351 % compute-bound, +1 841 % memory-bound, inside `extreme`, and the
  reason the step took 117 minutes instead of 15 (108 on `launch`).
  `gpu.kernel` +64.50 / +1.81 / +0.15 % and `gpu.api` +10.48 % launch-bound,
  inside their bands. **`gpu.profile` has no row**: it aborts the bench's
  native workload at exit on a double free inside `libomn_cupti.so`'s exit
  handler (reproduced 2 of 2 by hand; filed), so the run's assertion fails.
  `gpu.temp` reads +1.11 % on `compute`, over `low` (filed; a catalog
  change). The record in `docs/bench.md` still holds the first table, with
  the second beside it ("The second GPU table, after the nineteenth wave");
  `bench_gpu` and `bench_bands` green on it.
- **Item 4, skipped**: `sudo -n nvidia-smi -lgc` asks for a password.
  **Item 5, skipped**: the 3 ms `null_blk` load is not in the sudoers file.
- **Deferred to after the twentieth wave**: the matrix and
  `cargo test --test portability`, `scripts/bench.sh backends` and
  `bench_bands` on its table, and the full `make check`, so
  `portability::every_cell_covers_the_whole_catalog` stays red until then.

The commands of the eighteenth wave's runs, as they ran, are in this file at
`c6bd2b8`; what each run found stays here.

**Done** (2026-10-03): before the seventeenth wave, the bench's second table
(`scripts/bench.sh`, 68 minutes, its test green, 46 pairs over their band),
`netem` on lo (`join_engine::a_queued_packet_on_lo_...` passed for the first
time), and `null_blk` on the shared program pair (23.57 %, 781 ns a request);
earlier that day, the sixteenth wave's paranoid-2 list with a
capability-less binary, an installed tree (92 usable from
`/usr/local/bin/omn`), the `nosuid` advice, and `cost:` under
`bpf_stats_enabled` (39.7 us inside the target over 202 runs).

**Ran after the seventeenth wave's merge** (2026-10-03, at `718cae1`, alone;
`docs/open-findings.md`, "the seventeenth wave, coordinator, after the merge"):

- **Item 1, `null_blk`**: 12.03 % without program statistics (348 ns a
  request against a 1 % budget of 25.5 ns; was 23.57 % on the shared pair),
  14.47 % with them. Phase 6's criterion is still not met. With
  `bpf_stats_enabled` 1 the test reads *"the programs ran 0 times for 0 ns"*
  while the histogram counted every one of 26 838 511 requests: the statistics
  are read from the wrong programs, or after they are gone. And `chmod` right
  after `modprobe` races udev, which resets `/dev/nullb0` to 0600: wait for the
  node (2 s here) before the `chmod`.
- **Item 2, paranoid 2**: without capabilities, `cases` (4 passed, every case
  skipping with the machine's reason), `cross_memory` (10), the counted page
  faults (1) and `stack_capture`'s perf-map and python tests (5) are green,
  and the skip gate passes over all 28 skip lines. **With** capabilities,
  `cases` fails twice in two runs (green at paranoid -1): `make` grants the
  capabilities to `omn` and never to `omn-mcp`, so the agent's way runs
  unprivileged, `cpu.profile` user-mode only, at 280 % of `sqlite3`'s cpu,
  and stops on its budget. The same holds for an installed `omn-mcp`.

**Ran after the eighteenth wave's merge** (2026-10-04, at `29b1705`, alone,
with the sudoers grants; `docs/open-findings.md`, "The eighteenth wave,
coordinator, after the merge"). Items 1 to 5 of the list as it then stood ran; item 4's 3 ms load
and item 6 did not (neither is granted: the `completion_nsec=3000000` line
and `mount` are not in the sudoers file); the shim's saving before and after
`0a25793` was not measured (no recipe beyond the sentence below).

- **Item 1, paranoid 2, with the capabilities: green.** `cases` 6 passed:
  `both-ways-run-with-the-same-privileges` `ok` for all 15 causes run, every
  agent-way answer's `answered_by.file_capabilities` true (`cap_bpf`,
  `cap_perfmon`, `effective`) with nothing granted to `omn-mcp`, and no
  `sqlite3-durability` budget stop (the seventeenth wave's 280 % is gone).
  `a-cold-file` skipped both ways, its cause not there on this disk (handler
  off-cpu share 0.17 against 0.5), as it does at -1. The `FINDING` lines are
  the filed ones (`make-sqlite`'s one-job cause undecided on the person's way,
  the cited numbers, `cpu_queue`). The protocol test
  (`a_run_through_the_server_is_as_privileged_as_omn`) passed: the
  `CAP_PERFMON` half is shown. `family_sched` 17 and `scenario_sched` 52,
  `cross_memory` 10 (no `SKIPPED`: `CAP_PERFMON` lets it sample the kernel),
  the counted page faults, `stack_capture`'s five perf-map and python tests,
  and the two `--lib` tests passed (4096 faults weighed, 4096 taken).
- **Item 1, the user-only profile** (a capability-less copy, `sqlite3`'s 500
  commits): 5.95 % and 7.50 % of the target's cpu without debuginfod, 11.52 %
  with it, each degradation naming paranoid 2 and ending with its cost
  sentence. No first-check stop with debuginfod: its cache was warm, so the
  one-time load the open decision is about was not paid on this run.
- **Item 1, without the capabilities: the gate passes; one test is red.**
  `omn latency` over `hot.py` is refused, exit 1, with the machine's reason
  first (`sched.runqlat`: `CAP_BPF`, `unprivileged_bpf_disabled` 2, tracefs
  unreadable), and invents no names note. `cases` 6 passed with all 30 case
  lines skipping both ways on that reason; `.github/scripts/skip-gate.sh`
  passes over the 30. `family_sched` 17 passed; **`scenario_sched` 51 of 52**:
  `a_source_with_no_collector_is_refused_with_its_own_exit_code` asserts
  exit 3 and gets 1, because since the resolver stream's change the pinned
  `bpf:uprobe` is probed first and lacks `CAP_BPF` (filed). Phase 12's
  paranoid-2 criterion is judged met (`ROADMAP.md`).
- **Item 2, `null_blk`: 11.54 %, 332 ns a request** (bare 3 143 826 IOPS,
  2 781 156 under the histogram, a 1 % budget of 25.4 ns; every one of fio's
  27 814 345 reads counted but 3). With `bpf_stats_enabled` 1: 15.31 %, 457 ns
  a request, and **"the programs ran 53 622 158 times for 8 003 766 951 ns:
  149.3 ns per run, 298.5 ns per request counted"** -- twice the 26 811 063
  requests, so the statistics now read the programs that ran. `Q` is 65 % of
  that run's `N`; less the statistics' own clock reads (the 125 ns between
  the two runs' `N`), about 173 of 332 ns, still more than half. By the rule
  set before the run, most of it is the programs, and `tp_btf` is the next
  cut. The criterion is still not met; whether 1 % stays the criterion is
  still open (`DECISIONS.md`). `udevadm settle` before the `chmod` was enough:
  no udev race this time.
- **Item 3, the sched programs inside a target** (2000 `/bin/true` under
  `sh`): `sched.offcpu` 1.2 ms over 2046 runs of its own code (about 0.59 us
  a run, 0.89 % of the target's 1.26 s with the tool's cpu), `sched.runqlat`
  1.3 ms over 2151 runs (about 0.60 us a run, 1.76 %). Both inside `med`.
- **Item 4, the known-latency device: fails three of three** at
  `completion_nsec=6000`: the stream measured 7.2 to 12.3 us (median 7.5 us)
  for a device configured at 6 us, past the bucket's edge at 8192 ns. The
  timer's completion adds 1.2 to 6.3 us here, which the test's quarter-bucket
  margin does not include, so the granted load cannot serve it; the 3 ms load
  needs its sudoers line (filed).
- **Item 5, the bench's third table** (`target/bench/backends-1791084966/`,
  67 minutes, its test green, release build with its capabilities). Gate
  resolutions: launch 0.76 %, compute 0.18, memory 0.20, syscall 0.60, io
  0.86, contended 0.86, malloc 0.93. The same 95 rows as the second table.
  `cpu.profile` 4.69 % (was 4.94), `io.latency` 3.63 % (was 3.44), the heap
  rows lower (`mem.heap.live` 493 % from 732), `io.fsync` and
  `net.tcp.connect` through `bpf:fentry` 158 % and 44 % (their `stands`
  reason holds). Pasted into the record, `tests/bench_bands.rs` names **six
  bands outside**: `cpu.pressure` 0.15 %, `mem.cgroup` 0.11 % and `net.hwts`
  0.11 % against `free`'s 0.1 %, `net.tcp.rtt` through `netlink` 5.64 %
  against `med`'s 5 % (was 3.21), `power.freq` through `fs:sys` 1.19 % and
  `sched.migrations`' dilation 1.53 % on `contended` against `low`'s 1 %.
  Correcting them is a catalog change, so the record in `docs/bench.md` still
  holds the second table and the third is filed with the six (`docs/bench.md`,
  "The third table, after the eighteenth wave").
- **Not privilege**: `cross_machine::a_memory_limit_turns_...` and
  `join_engine::another_namespaces_lo_is_not_credited_to_this_ones` passed on
  `main`. `runner::a_profile_s_losses_land_...` **fails at load 1.0**, 73 298
  of 186 727 events short and the kernel throttling: a quiet machine is not
  enough, it is the gap filed, not the load. The stale `7.2.6-*` matrix cells
  were already gone.

---

## How this parallelizes

Eight waves of parallel agents have run on this project, and each was divided
differently because the work was shaped differently. What held every time:

- **The unit of ownership has to be a file set nobody else writes.** Wave one
  split by family and had no conflicts. Wave two split by shared layer and had
  a few, all in the one place two streams touched. Wave three had six streams
  append to one findings file and produced six conflicts on it. Wave four used
  a **file-name prefix** as the lock — `examples/<family>-*.yml`,
  `tests/scenario_<family>.rs`, `docs/scenarios/<family>.md` — and had zero.
- **The handoff must be a file only that stream can write.** A "write a log at
  the end" instruction produced no logs in wave three: every stream put its
  notes in the shared findings document instead. In wave four the document *was*
  the deliverable, owned and prefixed, and all eight arrived.
- **Registries are append seams and should be per family.** Collectors register
  in `src/collect/<family>/mod.rs`, so a new source is a line in a file one
  stream owns. A central `match` would be one conflict per agent.

The seventh wave divides **by question rather than by family**, and it is the
cleanest division so far: four crossings owning nothing but
`examples/cross-<name>-*.yml`, `tests/cross_<name>.rs` and
`docs/scenarios/cross-<name>.md`, and touching **no file under `src/` at all**.
That is what let a shared-layer stream run beside them rewriting
`src/bpf/{attach,btf,insn,map}.rs` and the catalog with no possibility of a
conflict. Four merges, four conflicts, every one of them in
`docs/open-findings.md` — the append seam the brief had named, resolved by
keeping both sides.

The eighth wave divides **by layer**, because its work was in `src/`
everywhere: the join engine, the run model, and the two attach kinds. The lock
went back to a named file list per stream, and it held for code -- four merges,
no conflict in a single source file. What it did not hold for is what the
coordinator did beside the streams: a field added to every bucketed row reached
into `net_softirq.rs`, which the fentry stream owned, and the merge produced a
softirq "vector 1760". Three programs had no layout test and that is where it
survived; all of them have one now. The lesson is recorded as finding 46 and it
is about the coordinator, not the streams: **a change made across every file of
a kind lands in files other people own**, and the only thing that makes that
safe is a test in every one of them.

Two seams were named ahead of time rather than discovered, which was the sixth
wave's lesson: that findings file, and the two session indexes. `examples/README.md`
and `GUIDE.md` belong to the coordinator, because after the sixth wave both were
twenty-eight sessions short for exactly the reason that every stream wrote
sessions and none owned the list. They are now held to the sessions on disk by
a test, in both directions — a commit had claimed that check existed and none
did, and the claim had already drifted by ten.

**Every earlier brief is now in `docs/briefs/`**, recovered from the history.
`BRIEF.md` is overwritten each wave and roughly thirty documents and tests cite
it — fifteen of them for "the four classes of `BRIEF.md`", which are the
scenario wave's and left that file two waves ago. `docs/briefs/README.md` states
the rule that resolves them: a citation in a document means the brief of the
wave that document belongs to.

---

## What to be careful of

Things that have already cost time, and will again.

**Capabilities do not survive a relink.** Every `cargo build` drops them, and
nothing announces it: the tool reports fewer sources as available, which reads
like the machine changed. `make` reapplies them.

**A test that skips is not a test that passes.** Three threads lost time to
this in one session, in three different shapes. A machine that cannot serve a
request and a request that cannot mean anything are different answers, and only
the first is a reason to stop quietly.

**A green suite on an unprivileged machine says nothing about the eBPF path.**
Those tests skip, loudly. The integration tests do take the privileged path once
`target/debug/omn` carries the capabilities; the in-process unit tests cannot.

**Measure before explaining.** Every surprising number in this project so far
has turned out to be right, and the expectation wrong: a fault is not a page,
memory the allocator already holds faults nothing, an idle share of 5.0 over
one tick is how the kernel credits residency to the tick a sleep *ended* in,
and a counter that counts kernel mode is not comparable to a user-mode
expectation.

**An aggregate is not a measurement.** Three collectors reported a machine-wide
sum or mean — `/proc/diskstats` added up, cpu frequencies averaged, idle
residency divided by the cpu count — and each of them hid exactly the thing it
was read to find. Averaging a series is something the emit layer does;
recovering a device or a core from an average is not, so the split always goes
towards keeping them apart.

**A symbolizer snapshots `/proc/<pid>/maps` when it is built**, and for a
launched program both obvious moments are wrong: at `start` the child has not
execed and the maps are this tool's own, and by the time its stacks are drained
it has often exited and there are none. The window in between is the first
tick.

**`cargo test` captures the print macros, not file descriptor one.** A suite
launched from a terminal has a terminal on stdout inside its own tests. Anything
that asks the process about its own environment is testing how the suite was
started.

**A measurement of a shared machine is not repeatable inside a parallel test
suite.** Nine hundred tests share this machine's caches, its SMT siblings, its
package power limit and its disks, and six scenarios had to be rewritten after
the fourth wave because each was asserting what the machine was doing rather
than what the tool did. The pattern of the fix is always the same: assert the
*separation* rather than a calibrated threshold, carry the reference's own
resolution as slack, and where the measurement genuinely cannot be taken, skip
loudly with the numbers that show why. A scenario that states its limit is
worth more than one that hides it.

**A test must not assert which way a race went.** `proc.fd` on a launched
program has three outcomes depending on whether the child reached `execve` and
whether it had opened its files — and the defect is that the answer is
*unspecified*, so that is what the test asserts, over several runs.
