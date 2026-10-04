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
every source. Ninety-six sources across nine families collect for real, over
eight mechanisms: procfs and sysfs, `perf_event_open` counting and sampling,
netlink, an `LD_PRELOAD` shim, NVML through `dlopen`, `af_packet` with a
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
default `perf_event_paranoid`.

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

**2271 tests passed** in the eighteenth wave's last full run (`cargo test
--workspace --no-fail-fast` at `cc3adc1`, the first of its two merges), 11
ignored, and **`make check` is red on one test**:
`runner::a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed`,
which fails every time on a machine loaded enough to throttle a 50 kHz
profile, because `cpu.profile`'s `losses_by_ring` carries each ring's drops
and none of its throttles. It is a gap in the tool, not a flake, red on the
wave's base as well, and filed for the profile's owner (`docs/open-findings.md`,
"the eighteenth wave"). The other three failures of that run were a test in
the flaky tally and one `scenario_net` timing failure counted twice, each
green alone. The second merge (`f51189d`) had the library's 1482 tests and
every other binary green but that one. The seventeenth wave's last
`make check` was green, 2191 passed (`ae889f7`). The ignored are
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

What is missing is breadth, not skeleton, and very little of it is left: **five
sources have no collector**, and each waits on something this machine does not
have or this project has not chosen -- Intel PT on an AMD part, an uncore PMU, an
XDP program, and NVIDIA's CUPTI.

---

## What the numbers say

```
101 sources: 92 usable here, 0 unverified, 9 out of reach on this machine.
96 of them have a collector, covering 108 of their 205 backends.
```

(`target/debug/omn list` on `main` at `f51189d`, after the eighteenth wave's
merges, unchanged from `ae889f7`, with `CAP_BPF` and `CAP_PERFMON` on the binary as `getcap` shows them,
at `perf_event_paranoid` -1; 56 usable from the same binary relinked without
them, the matrix's `here-nocaps` cell, swept in the sixteenth wave. Unchanged
since the fifteenth wave. The 108th backend is `cpu.cycles` on `pmu:sw`, the cpu clock as the
fallback for a cpu with no hardware PMU.)

Three different questions, deliberately kept apart.

| Question | Answer | Meaning |
|---|---|---|
| What can this machine serve? | 92 | probed now, **through a backend this build collects**: the file is readable, the event opens, the tracepoint is there |
| What can this build collect? | 96 sources, 108 backends | a collector is written and registered |
| What is neither? | the rest | vocabulary: it resolves, it validates, and it says so |

That `92` is with `CAP_BPF` and `CAP_PERFMON` on the binary, and it is why
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
| `gpu` | 7 | 9 | 6 |
| `proc` | 6 | 6 | 6 |
| `power` | 5 | 5 | 4 |
| `runtime` | 4 | 4 | 4 |

**`io`, `sched`, `proc`, `power` and `runtime` are complete**, `net` gained
`net.request` for the vertical case, and the rest are one or two short. The
section below names what the last five wait for.

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
| `gpu.power --mode count --emit rate` | NVML's energy counter at each end of a window | the board's mean draw in watts, against `nvidia-smi` sampled from another process over the same window (21.09 W against 20.47 W) |
| `--container NAME` | the cgroup its runtime names | 388 blocks kept of the task inside, 387 by the kernel's count, 0 of the task beside |
| `sched.offcpu --emit stack` | eBPF `bpf_get_stackid` into a stack map | the verifier accepting the hand-assembled program, and a 300 ms sleep coming back with the twelve frames that led to it |
| `sched.runqlat` | eBPF, bucketed in the kernel | a known number of blocking round trips, found by four sources at once |
| `sched.migrations` | eBPF classic tracepoint | two hundred migrations driven on purpose with `sched_setaffinity` |
| `mem.pressure`, `.swap`, `.thp`, `.cgroup`, `.numa`, `.wss` | procfs, sysfs, cgroup2 | per-node residency against a known allocation; PSI against the kernel's own stall counters |
| `mem.cache_l1`, `.cache_llc`, `.cache_tlb` | `perf_event_open`, `PERF_TYPE_HW_CACHE` | a shuffled walk over 64 MiB against one over 4 KiB, and the descriptor packing checked field by field |
| `cpu.oncpu_time`, `cpu.util` per core | procfs, per-cpu perf | a spin loop pinned to one core with `taskset`, started from outside the tool |
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
| 5 — generalizing the join engine | **done**, and a fifth kind since the eighth wave. The hour of joins ran to its end in the sixteenth wave (1500 rounds, nothing live, 0.7 MiB a round), `overlap` takes `enclosing:` for a drained row (and a stall row carries its last end since the seventeenth), and `iface` is joined by its namespace and its index there since the seventeenth wave. `interval`, `eq`, `scoped_eq` and `wakeup` each have a producer; `between` is the one three crossings asked for independently, and it is `TEMPORAL` by construction — a polled total's change credited to what moved with it, which the ladder must not call `KEY`. Beside the joins, a `decompose:` block with a signed residue that is never rounded away. Since the eighteenth wave a rule chooses its tier: `query` applies the same engine to a stored run's `raw` events (`join::apply_stored`), `kernel` is refused with the reason until a match kind has a program; no command applies a rule to a stored run yet. |
| 6 — emit, aggregation, budget | **done but for one measurement**: the 2 M IOPS overhead of the in-kernel histogram. Run once before the seventeenth wave, it read 23.57 % (781 ns a request at 3.16 M IOPS); the wave gave the histogram its own program pair, whose cost is the coordinator's next `null_blk` run, and whether 1 % is the right criterion is open (`DECISIONS.md`). The eighteenth wave found why the program statistics read 0 runs (the block programs were closed at the stop, before the run read their account) and keeps them loaded until the close. Run after its merge: 11.54 %, 332 ns a request, of which the programs' own run time is most (298.5 ns a request with the statistics on, which add about 125); `tp_btf` is the next cut, and the criterion is still open. A kernel-built row's sum is its exact `total_ns` in every emit since the seventeenth wave. Since the sixteenth wave the budget's ladder steps from `trace` to `sample` on a collector that weighs its samples (`mem.faults` on `pmu:sw`), held to the task's own fault count across the move, and weighs the target in nanoseconds. Every emit exists, and every declared output format has an encoder — `folded`, `speedscope`, `pprof` and `chrome` included. A run measures its own cpu against the target's and is held to `--budget`, degrading or stopping and saying which. And since the tenth wave, **hot attach**: a request carries a `window` inside the run, attached and detached while the rest keep running, and two sources attached for one phase each of a program count that phase within +0.17 % and nothing of the other. Three edges are filed with the files that own them: the join engine credits events after a detach to a stale state, the results document does not yet say where a window was, and a per-task counter attached late to a program that already has threads counts one of them. |
| 7 — declarative configuration | **done**, every criterion: since the sixteenth wave's sweep every gap between two fresh matrix cells is explained by their resolved records. Sessions in YAML, JSON and TOML; a dumped request reads back unchanged; the resolved file is written beside the results. |
| 8 — self-benchmark | **done**; the second table ran before the seventeenth wave (68 minutes, its test green, 46 pairs flagged over their declared band, worst `io.fsync` through `bpf:fentry` at 142 %), and since the eighteenth wave the bands are corrected to it: 43 `cost:` lines moved, two stand with their reason, the table is recorded in `docs/bench.md` and `tests/bench_bands.rs` fails when a band and its row disagree. A table re-measured after the wave's fixes is the next run. The harness has every task the phase lists, and it has measured: 94 (source, backend) pairs alone on this machine, every workload admitted by a gate that judges what a median resolves and refuses a busy machine (`docs/bench.md`). The resolver's seven choices between served backends were confirmed. What the table corrects is the cost bands: an in-kernel collector's cost is in the target's dilation, not the tool's cpu (`proc.syscall`: 0.42 % against +296 %), and the preload shim cost an allocation-bound program about 150 times -- two system calls a record -- until it was moved onto a shared ring, which brought a record to about 70 ns. Finding 47 files each. |
| 9 — breadth | nearly done; **96 of 101 sources, 108 of 205 backends**. The five without a collector wait on hardware this machine lacks or on a vendor library. |
| 10 — intelligence layer | **its first method is done by the engine**. TSA — where a thread's wall clock went — is what `examples/join-decompose-compute.yml` produces: three states, each row's kind and resolution, the program's life as the whole, and a residue inside its own resolution. Since the tenth wave the layer above it exists and calls the engine like any other user: the objective is required and refused when missing, `omn latency` names what dominates a run or its tail, `omn saturate` reads a queue by Little's law, and `omn diff` subtracts two profiles -- each checked against a workload whose limit the test chose. Since the eleventh wave **the escalation** closes it: a session's `escalate:` block attaches the layer the cheap pass names mid-run, detaches the rest and holds the budget; a program cpu-bound then lock-bound gets the lock layer 0.4 to 0.5 s after the switch (`docs/triage.md`). **Done.** Since the eighteenth wave `omn latency` follows a wait for a child into the child that ran during it and names the busiest other thread beside a block it ran in, asks `sched.mutex` only in a second pass, and takes `--symbols`. |
| 11 — a model reads it | **done** (the twelfth wave): `omn-mcp`, an MCP server over stdio in the workspace, exposing the catalog, the runs a directory holds and this machine's sweep, as the documents themselves regrouped per request with what each is missing stated first. It probes nothing (the `omn` binary answers every machine question), and runs a session only when its operator allowed it, bounded, budgeted, and never one that launches a program (`docs/mcp.md`). What it filed: `omn list --format json` carrying the sweep, so the server stops parsing the text table. Since the fourteenth wave the use case runs end to end: an agent benchmarks a launched program through approved sessions and says what dominates its time, every number it is handed held to the run's files (`docs/mcp.md`, "The walk-through"). Since the eighteenth wave one privilege model: the capabilities are on `omn` alone and `omn-mcp` collects through it, shown on a `CAP_BPF` run; `run_session` takes an optional budget, says what a budget is a share of (`budget_against`) and hands back the program's stdout. |
| 12 — programs nobody wrote for it | **started in the seventeenth wave**: a case is a directory (`cases/<name>/case.yml`, `docs/cases.md`), asked both ways by `tests/cases.rs`, `omn latency` and `omn-mcp`. Seven cases since the eighteenth wave brought `gcc-shell` back (`sqlite3-stripped`, `sqlite3-durability`, `cpython-interpreter`, `cpython-perf-map`, `http-server-stdlib`, `make-sqlite`, `gcc-shell`). The first suspect is the known cause, both ways, for every cause asked here, `make-sqlite`'s one-job cause in four runs of five; what still gets the cited numbers wrong (a job is a chain of processes, a thread that waits beside the main thread) is filed. Three of six exit criteria met (a seventh program is a directory, a stripped binary named from debuginfod both ways, and every case at paranoid 2: run after the merge, each cause answers as at -1 with the capabilities and skips both ways with the machine's reason without them, through the skip gate); `ROADMAP.md` says which are partly. |

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

**After the eighteenth wave, in order:**

1. **The privileged runs the handoffs asked for, alone** -- **ran on
   2026-10-04** except the 3 ms `null_blk` load, the cold file and the shim's
   saving; what they found is in "Needs privilege" below, with the exact commands: every case, `mcp`'s protocol test, the
   profile and the sched files at `perf_event_paranoid` 2, with and then
   without the capabilities, through the skip gate (cases, mcp, profile,
   names, sched; phase 12's paranoid-2 criterion waits on it); the in-kernel
   histogram on `null_blk` with the statistics that now read the programs that
   ran (phase 6's criterion and its open decision); what the sched programs
   cost inside a target, under `bpf_stats_enabled`; the known-latency
   `null_blk` test (a sudoers line first); the bench re-measured after the
   wave's fixes, its table pasted into `docs/bench.md` and held by
   `tests/bench_bands.rs`; and, not privileged but on the quiet machine, the
   engine's reclaim and namespace tests on `main` and the shim's saving.
2. **The push, and the CI read back.** Everything since `b37a925` is
   unpushed. `git push`, `gh run watch`, and `.github/scripts/skip-gate.sh`
   over a red cell's `target/test.log`. What only that run shows: the bpftool
   download and v7.7.0 dumping the azure kernel's BTF; `cross_machine`'s
   memory-limit test past its cgroup; the swap children under `memory.high`
   on the cloud disk; the 128 MiB working set on the runner's cpu;
   `scenario_io`'s queue through `io.inflight`'s two accounts; `encoders` #27;
   and `gcc-shell` in the cell, which needs `gcc`, `unzip`, `taskset` and the
   fetch from sqlite.org (filed for `.github/**`).
3. **`make check` green**: `cpu.profile`'s `losses_by_ring` carrying each
   ring's throttles (per-ring counts from `src/perf.rs`, a shared layer), so
   `runner::a_profile_s_losses_land_...` passes. Until then `make check` is
   red on it on any machine loaded enough to throttle.
4. **The triage, from what the re-asked cases found** (`docs/fixes/w18-cases.md`
   section 3; `src/triage/latency.rs`): **a job is a chain of processes** --
   the wait's place taken by the child it names and the processes under it,
   not its busiest descendant (`make-sqlite`, `gcc-shell`'s numbers); **a
   thread that waits beside the main thread** -- the serving thread blocked in
   its send (`http-server-stdlib`'s slow client); the tool's own eBPF frames
   (`bpf_trace_run2`) never the named suspect; one spelling of `cpu-queue` and
   `other-block`; `pid` on profile samples and the late discovery (a child's
   on-cpu row 14 % short); the lossy-profile verdict undecided under load
   (`triage::a_verdict_read_off_a_lossy_profile_...`, red on the base under
   load, green in phase B's run).
5. **The budget and the run's account**: a first check that stops a run on a
   one-time cost (debuginfod's symbol load, `bpf:fentry`'s 55 ms attach) --
   the open decision; the symbol table loaded lazily, and a kernel frame not
   named per sample (`src/stack.rs`: about 150 us a sample with both halves);
   the in-target meters read before the stops (about twenty collectors close
   their programs in `stop`); a session naming every request refused at
   resolution (`docs/scenarios/sched.md` defect 8); the launched program's own
   stderr; `run_session` saying the budget's stop; an orphan that left the
   group before a stop (a subreaper, if a case shows one).
6. **The shared layers and the files nobody owned this wave** (the
   coordinator's): `assemble` skipping a query-tier rule and cloning only
   `join::sources_read` (the diff in `w18-engine.md`); `NamedBy` kept in
   `Results::stacks` and stack rows keyed by the request's keys (`src/emit.rs`);
   `cheaper_equivalents` into the catalog loader; `footprint.rs` reading again
   after an ESRCH from a running process; `tests/cross_machine.rs`'s cgroup
   named per call; `net_qdisc`'s namespace offset through `member_offset`; the
   executable of each task a result keys; an `omn join` command over a stored
   run, with `tier:` in `man/omn.1`.
7. **Documents that lag the code**: `docs/mcp.md` (the budget optional,
   `program_stdout`, `budget_against`, the protocol test table, reading a
   followed task's table, `raw` beside `stack`); `CATALOG.md`'s cost column
   for the bands bench moved and its sched section (children followed, the
   kernel's wait); `examples/sched-round-trips.yml`'s and
   `sched-half-blocked.yml`'s headers; `proc-not-built.yml` exiting 1 or 3
   (`docs/scenarios/proc.md:161,182`, `GUIDE.md:362`,
   `examples/README.md:191`, and the prose around the renamed tests in
   `docs/scenarios/io.md` and `gpu.md`); `GUIDE.md`'s latency session;
   `docs/unwind.md`'s perf-map line; the bench harness's `judge_band` reading
   a dilation within the workload's resolution as zero; `tests/cases.rs`'s
   `MODEL_BUDGET` and `ask_mcp` fallback.
8. **Still open from earlier waves**: a known number of merges; one program
   for both `io.latency` histograms (optional); `io.inflight`'s cause on
   6.17; `sched.lock` and `sched.futex` following descendants;
   `unwind`'s stripped-build second shape (seen once more under load); a
   sweep from an installed tree; the stale `7.2.6-*` matrix cells; registered
   extractors for the build's three accounts (optional); the flaky tally's
   new entries (`docs/open-findings.md`, "The eighteenth wave, the
   coordinator's").
9. **The matrix**, swept alone at the end of the next wave: the sched
   fallbacks on kernels before 6.16, and the `member_offset` audit on the
   other kernels.

Below the milestones, and not scheduled:

- **CTF import** — ingest an LTTng-UST stream and merge it into the timeline.
  A consumer of one more format, not a source: `DECISIONS.md` boundary 6 says
  why LTTng is a peer rather than a backend. Low priority; nothing depends on
  it and no measurement here is worse without it.
- **Two decisions are open** (`DECISIONS.md`): the phase 6 criterion for
  the in-kernel histogram, pending its run, and what the budget's first check
  may stop a run on (a one-time cost weighed against a few milliseconds of
  target). Which task the triage decomposes, and the order of "unbuilt" and
  "missing", are settled and in the code since the eighteenth wave.
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

### 2. What is left: five sources, and none of them is this machine's to serve

The ninth wave took six of the eleven -- the page cache, the preload shim
pointed at `pthread_mutex_lock` and the GL/CUDA entry points, and AMD IBS -- and
what the last five wait on is not a collector nobody has written:

| Waits for | Sources |
|---|---|
| uncore PMUs | `mem.bandwidth` |
| Intel PT, and this machine is AMD | `cpu.flow` |
| an XDP program, the one type here that can drop packets rather than observe them | `net.xdp` |
| CUPTI, Nsight | `gpu.kernel`, `gpu.counters` |

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
coordinator, after the merge"). Items 1 to 5 below ran; item 4's 3 ms load
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

The commands, as they ran (kept for the next run), in the order that changes
the machine least between them:

1. **`perf_event_paranoid` 2, as the CI's `defaults` cell runs** (cases
   section 5, mcp section 6, profile, names section 4, sched section 5, and
   the seventeenth wave's list). One sysctl flip for all of it: with the
   capabilities first, then from a relinked binary without them, through the
   skip gate.

   ```console
   cd /home/moha/repos/omniscient && make
   sudo -n /usr/bin/sysctl kernel.perf_event_paranoid=2
   getcap target/debug/omn target/debug/omn-mcp       # cap_bpf,cap_perfmon on omn, nothing on omn-mcp
   # with the capabilities: every cause answers as at -1
   OMN_CASES_KEEP=target/cases-p2-caps cargo test --test cases -- --nocapture 2>&1 | tee target/cases-p2-caps.log
   getcap target/debug/omn target/debug/omn-mcp       # unchanged, or the earlier failure was a relink
   cargo test -p omn-mcp --test protocol a_run_through_the_server_is_as_privileged_as_omn -- --nocapture
   cargo test --test family_sched --test scenario_sched 2>&1 | grep -E 'skipped|test result'
   cargo test --test cross_memory -- --nocapture 2>&1 | grep -E 'SKIPPED|eight large|test result'
   cargo test --test scenario_mem a_counted_run_of_page_faults -- --nocapture
   cargo test --test stack_capture -- perf_map python
   cargo test --lib collect::cpu::profile::tests::a_tick_with_no_sample_of_a_process_does_not_reread_its_mappings
   cargo test --lib collect::mem::page_faults::tests::sampled_in_user_mode_the_weighed_samples_are_the_faults_the_thread_took
   # what a user-only profile costs (profile): the CI's defaults cell, a copy with no capabilities
   cp target/debug/omn target/omn-nocap
   cd "$(mktemp -d -p ~/.cache)"                       # on a disk, not tmpfs
   python3 /home/moha/repos/omniscient/cases/sqlite3-durability/commits.py c500.sql 500
   sqlite3 delete.empty.db 'PRAGMA journal_mode=DELETE; CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT);'
   for symbols in "" "--symbols debuginfod"; do
     cp delete.empty.db delete.db
     /home/moha/repos/omniscient/target/omn-nocap cpu.profile -m sample@997hz --stack fp $symbols -e topk --budget 100% \
       -- sqlite3 "$PWD/delete.db" 'PRAGMA synchronous=FULL' ".read $PWD/c500.sql" 2>&1 \
       | grep -E 'degraded: stacks|cost:|budget:'
   done
   cd /home/moha/repos/omniscient
   # without the capabilities: every cause skips both ways with the machine's reason first
   touch src/main.rs && cargo build --workspace && getcap target/debug/omn   # (empty)
   target/debug/omn latency --objective latency -- python3 -X perf cases/cpython-interpreter/hot.py <input>
   OMN_CASES_KEEP=target/cases-p2-bare cargo test --test cases -- --nocapture 2>&1 | tee target/cases-p2-bare.log
   .github/scripts/skip-gate.sh target/cases-p2-bare.log .github/expected-skips.txt
   cargo test --test family_sched --test scenario_sched 2>&1 | grep -E 'skipped|test result'
   sudo -n /usr/bin/sysctl kernel.perf_event_paranoid=-1 && make
   ```

   What to read. With the capabilities: `both-ways-run-with-the-same-privileges`
   `ok` for every cause, each agent-way answer's
   `answered_by.file_capabilities` true, and no `sqlite3-durability` budget
   stop -- if there is one and `getcap` held, `budget_against` says which
   collector, and it is the first check's one-time load (open decision); the
   protocol test shows the `CAP_PERFMON` half of one privilege model;
   `cross_memory` prints no `SKIPPED` and *"faults sampled in user mode only
   (perf_event_paranoid 2)"* (if its lower bound fails there, it keeps only
   the upper bound, `docs/fixes/w17-core.md` section 3). The profile runs:
   the degradation names paranoid 2 and ends with its cost sentence; without
   debuginfod the run finishes under 15 %; with it the first check stops it
   above 100 % until the one-time load is weighed apart. Without the
   capabilities: the `omn latency` run degrades with the machine's reason and
   invents no names note; every case skips both ways with the same reason;
   the gate passes. Phase 12's paranoid-2 criterion is judged from this run.

2. **The in-kernel histogram's cost on `null_blk`** (io; phase 6's last
   criterion and its open decision). The statistics now read the programs
   that ran, so the second run's *"the programs ran R times"* must show `R`
   near twice the requests counted.

   ```console
   cd /home/moha/repos/omniscient && make
   sudo -n /usr/bin/modprobe null_blk queue_mode=2 irqmode=0 completion_nsec=0 nr_devices=1 gb=4 bs=4096 hw_queue_depth=256 submit_queues=8
   udevadm settle --timeout=10
   sudo -n /usr/bin/chmod a+r /dev/nullb0
   cargo test --test scenario_io -- --ignored --exact \
       the_in_kernel_histogram_costs_a_two_million_iops_load_under_one_percent --nocapture
   sudo -n /usr/bin/sysctl kernel.bpf_stats_enabled=1
   cargo test --test scenario_io -- --ignored --exact \
       the_in_kernel_histogram_costs_a_two_million_iops_load_under_one_percent --nocapture
   sudo -n /usr/bin/sysctl kernel.bpf_stats_enabled=0
   sudo -n /usr/bin/rmmod null_blk
   ```

   Read *"... X %, N ns per request against a 1 % budget of B ns"*, then
   *"the programs ran R times for T ns: P ns per run, Q ns per request
   counted"*. Q against N decides: a small share of it makes the criterion a
   per-request cost; most of it (io expects 150 to 300 ns of the 348) makes
   `tp_btf` the next cut. The result goes into `docs/scenarios/io.md` defect
   19, this file, phase 6 and `DECISIONS.md`.

3. **What the sched programs cost inside a target** (sched section 5), in the
   same `bpf_stats_enabled` window or its own: the fork and free programs on
   every fork and free of the machine, `sched_entry_tp` on every
   `__schedule`.

   ```console
   sudo -n /usr/bin/sysctl kernel.bpf_stats_enabled=1
   target/debug/omn trace sched.offcpu -e summary -- /bin/sh -c 'for i in $(seq 2000); do /bin/true; done'
   target/debug/omn trace sched.runqlat -e summary -- /bin/sh -c 'for i in $(seq 2000); do /bin/true; done'
   sudo -n /usr/bin/sysctl kernel.bpf_stats_enabled=0
   ```

   Read the `cost:` line's *"inside the target"* figure for both: whether
   `sched_entry_tp` is worth its 0.65 us a block.

4. **A device of known latency** (io, defect 22; the sixteenth wave's
   item). `a_device_with_a_known_latency_puts_every_request_in_its_bucket_in_both_histograms`
   is written and has never run. It reads the latency from the module's own
   parameters, so the load already granted (`irqmode=2
   completion_nsec=6000`, 6 us, 1.9 us from its bucket's edge) should serve;
   the stream asked for `completion_nsec=3000000`, which needs a line in
   `/etc/sudoers.d/omniscient-privileged` first: `/usr/bin/modprobe null_blk
   queue_mode=2 irqmode=2 completion_nsec=3000000 nr_devices=1 gb=1 bs=4096`.

   ```console
   sudo -n /usr/bin/modprobe null_blk nr_devices=1 queue_mode=2 irqmode=2 completion_nsec=6000 gb=1
   udevadm settle --timeout=10
   sudo -n /usr/bin/chmod a+r /dev/nullb0
   cargo test --test scenario_io -- --ignored --exact \
       a_device_with_a_known_latency_puts_every_request_in_its_bucket_in_both_histograms --nocapture
   sudo -n /usr/bin/rmmod null_blk
   ```

5. **The bench, re-measured after the wave** (bench), alone, 68 minutes the
   last time:

   ```console
   cd /home/moha/repos/omniscient && make && scripts/bench.sh backends
   ```

   Then the new table into the record in `docs/bench.md` (each row's
   `% of target` and dilation medians, the gate's resolution, the band
   declared, the verdict) and `cargo test --test bench_bands`, which names
   every band the table puts outside and every `stands` whose reason is gone.
   `cpu.profile` is the row most likely to move back down, `io.latency` up.

6. **The cold file on slow storage** (python, from the seventeenth wave;
   optional, a functional look): `http-server-stdlib`'s `a-cold-file` on a
   `dm-delay` device of 50 ms reads over a loop device
   (`docs/fixes/w17-python.md` section 6; `mkfs.ext4`, `mount` and `chown` are
   not in the sudoers file, so `docs/scenarios/io.md` section 18's recipe is
   the one to adapt).

Not privilege, but for the same quiet machine after `make` (engine): the
reclaim crossing and the namespace test on `main`, now that both landed, and
the shim's saving on a `malloc`/`free` loop under a `sched.mutex`-only
session, before and after `0a25793`, with the release shim:

```console
cargo test --test cross_machine a_memory_limit_turns -- --nocapture
cargo test --test join_engine another_namespaces_lo_is_not_credited_to_this_ones -- --nocapture
```

And `runner::a_profile_s_losses_land_...` once on the quiet machine, to see
whether it passes when nothing throttles the profile. The bench's `setcap` on
`target/release/omn` is granted, so `scripts/bench.sh` needs nothing typed.

Not privilege either: **the five stale matrix cells**, `rm -r
target/matrix/cells/7.2.6-*`. The next matrix sweep is at the end of the next
wave, alone.

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
