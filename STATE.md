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

**2123 tests passed** in the fifteenth wave's merge run (`cargo test
--workspace --no-fail-fast`), ten more ignored, and four failed: two tests the
kernel stream's per-backend modes on `net.hwts` made stale at the merge (fixed
in `fbe6f19`), one known flake that passed alone
(`scenario_sched::a_forced_switch_program_splits_the_switches_as_the_kernel_counts_them`),
and the portability judge reading cells swept before this wave's binary
(cleared by the end-of-wave matrix sweep, "What remains" below). The ignored are
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

(`target/debug/omn list` on `main` at `fbe6f19`, after the fifteenth wave's
merge, with `CAP_BPF` and `CAP_PERFMON` on the binary as `getcap` shows them,
at `perf_event_paranoid` -1; 56 usable from a copy of the binary that carries
none. The 108th backend is `cpu.cycles` on `pmu:sw`, the cpu clock as the
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
| 4 — the vertical case | **done, except `dm-delay`, which needs root.** One request decomposed boundary by boundary on a real server, the lock's holder and the block device named, and since the tenth wave the tree's two network rows: a client that pauses 30 ms mid-request puts 30.14 to 30.27 ms in `net_rx`, and a slow reader puts 32.8 to 33.8 ms in `net_tx`, both injected from the client without root. The rows sum to the client's latency within +0.06 % to +0.36 % (+3.1 % for the slow reader, whose last buffer the client reads after the server is done). The network rows print `IDENTITY` where they are `KEY`; the engine change is filed. |
| 5 — generalizing the join engine | **done**, and a fifth kind since the eighth wave. `interval`, `eq`, `scoped_eq` and `wakeup` each have a producer; `between` is the one three crossings asked for independently, and it is `TEMPORAL` by construction — a polled total's change credited to what moved with it, which the ladder must not call `KEY`. Beside the joins, a `decompose:` block with a signed residue that is never rounded away. |
| 6 — emit, aggregation, budget | **partly**. Every emit exists, and every declared output format has an encoder — `folded`, `speedscope`, `pprof` and `chrome` included. A run measures its own cpu against the target's and is held to `--budget`, degrading or stopping and saying which. And since the tenth wave, **hot attach**: a request carries a `window` inside the run, attached and detached while the rest keep running, and two sources attached for one phase each of a program count that phase within +0.17 % and nothing of the other. Three edges are filed with the files that own them: the join engine credits events after a detach to a stale state, the results document does not yet say where a window was, and a per-task counter attached late to a program that already has threads counts one of them. |
| 7 — declarative configuration | **done**. Sessions in YAML, JSON and TOML; a dumped request reads back unchanged; the resolved file is written beside the results. |
| 8 — self-benchmark | **done.** The harness has every task the phase lists, and it has measured: 94 (source, backend) pairs alone on this machine, every workload admitted by a gate that judges what a median resolves and refuses a busy machine (`docs/bench.md`). The resolver's seven choices between served backends were confirmed. What the table corrects is the cost bands: an in-kernel collector's cost is in the target's dilation, not the tool's cpu (`proc.syscall`: 0.42 % against +296 %), and the preload shim cost an allocation-bound program about 150 times -- two system calls a record -- until it was moved onto a shared ring, which brought a record to about 70 ns. Finding 47 files each. |
| 9 — breadth | nearly done; **96 of 101 sources, 108 of 205 backends**. The five without a collector wait on hardware this machine lacks or on a vendor library. |
| 10 — intelligence layer | **its first method is done by the engine**. TSA — where a thread's wall clock went — is what `examples/join-decompose-compute.yml` produces: three states, each row's kind and resolution, the program's life as the whole, and a residue inside its own resolution. Since the tenth wave the layer above it exists and calls the engine like any other user: the objective is required and refused when missing, `omn latency` names what dominates a run or its tail, `omn saturate` reads a queue by Little's law, and `omn diff` subtracts two profiles -- each checked against a workload whose limit the test chose. Since the eleventh wave **the escalation** closes it: a session's `escalate:` block attaches the layer the cheap pass names mid-run, detaches the rest and holds the budget; a program cpu-bound then lock-bound gets the lock layer 0.4 to 0.5 s after the switch (`docs/triage.md`). **Done.** |
| 11 — a model reads it | **done** (the twelfth wave): `omn-mcp`, an MCP server over stdio in the workspace, exposing the catalog, the runs a directory holds and this machine's sweep, as the documents themselves regrouped per request with what each is missing stated first. It probes nothing (the `omn` binary answers every machine question), and runs a session only when its operator allowed it, bounded, budgeted, and never one that launches a program (`docs/mcp.md`). What it filed: `omn list --format json` carrying the sweep, so the server stops parsing the text table. Since the fourteenth wave the use case runs end to end: an agent benchmarks a launched program through approved sessions and says what dominates its time, every number it is handed held to the run's files (`docs/mcp.md`, "The walk-through"). |

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

**After the fifteenth wave, in order:**

1. **The end-of-wave matrix sweep, alone on a quiet machine.** Every cell on
   disk was swept by a binary older than this wave's (`here-nocaps` on
   2026-09-29, the rest on 2026-09-30), so none records the privileges, the
   virtualization or the binary that swept it, and all 22 are to be rewritten:

   ```console
   make                        # the capabilities, checked with getcap
   scripts/matrix.sh here      # here-caps, here-nocaps
   scripts/matrix.sh           # 5.15.221, 6.1.188, 6.6.157 and 7.2.6, under caps, root, nocaps, nobtf, notracefs
   cargo test --test portability
   ```

   What should move:

   | Cells | What should change | Whose |
   |---|---|---|
   | every cell | `privileges`, `virtualization` and `cell.build` recorded; of the 2754 gaps between cells counted as unrecorded, none left (each explained or failed); the 83 differences between cells of different builds gone | probes |
   | `here-caps` | `net.hwts --system -m trace` passed over at resolution (`netlink:ethtool` serves `poll`, `snapshot`), the sweep moves to `poll`; the judge's one violation there gone | kernel |
   | `here-nocaps` | the five stale "usable through a backend with no collector" violations (`cpu.profile` through `pmu`, `cpu.oncpu_time` and `io.bytes` through `fs:cgroup`, `io.latency` through `fs:proc`, `gpu.kernel` through `vendor:cupti`) gone; 56 usable, as this binary lists without capabilities | probes |
   | any cell with no hardware PMU | `cpu.cycles` usable through `pmu:sw`, degraded *"CPU time, not cycles"* | probes |
   | `caps`, `root`, every kernel | `sched.switches --cgroup -m trace` exit 2 with each backend's half, not 1 | probes, resolver |
   | every cell | `--container` accepted wherever a cgroup is served (the runtime's own answer where there is none) | probes |

   `portability::every_cell_on_disk_holds_the_invariants` is red until then,
   with the six violations above. Then **the next push's CI**, both cells,
   read back (the fourteenth wave's list still applies, plus the bpftool
   cross-check's new skip wording, filed to `.github/expected-skips.txt`).
   And once, **a sweep from an installed tree** (`make release && sudo make
   install PREFIX=/usr/local`, the `setcap` line it prints, then
   `/usr/local/bin/omn list`: 92 here, as `target/debug/omn`).
2. **What is open in the streams' own files**, each with its reason in the
   handoff:
   - the join hour, run for 300 s and not 3600 (engine; a quiet machine);
     the engine is applied after the run, so its memory is linear in the run;
     a rule cannot choose the kernel or query tier (phase 5);
   - the 2 M IOPS overhead of the in-kernel histogram, written and not run
     ("Needs privilege" below); edge-crossers between the streamed and the
     kernel histogram (4 of 318 in one of five runs);
   - phase 6's mode ladder (trace -> hist -> sample) and the budget's
     denominator in nanoseconds, both waiting on a decision (`DECISIONS.md`,
     "Open"); the bench's second table, which alone exercises the band column;
   - `cpu.cycles` through `pmu:sw` reports nanoseconds in the entry's unit
     (`count`), said on the `degraded:` line and in the counter's name; a
     per-backend unit needs `Backend.unit` in `src/catalog.rs`;
   - `FilterFieldNotCarried` picks its `elsewhere` the old way, with no check
     that the backend is collected or runnable here (`src/resolve.rs`; no case
     found to test it against);
   - debuginfod never tried against a real server (loopback only, no TLS).
3. **What the fifteenth wave filed and is still open**, under its six headings
   in `docs/open-findings.md` ("Needs from another stream — the fifteenth wave,
   stream X"), checked against `main` at `fbe6f19`. Landed at the merge and so
   closed: the `decompose_within` swap and `check_paired_parts` removed
   (`303fcfe`), the triage engine's resolver arms (`c481b1c`), the unwind
   table's name in three documents (`65577bf`), `net.hwts`'s modes (kernel),
   `Cost::Extreme`'s doc (resolver), the refusal leading with the backend passed
   over (resolver), `CATALOG.md:510`'s container line and `CLAUDE.md`'s counts
   (this commit). Still open, by the files they land in:
   - **`src/collect/mod.rs`**: `pub mod psi` moved where it lives, with the
     kernel stream's three lines (one commit across two owners); `mod io` to
     `pub(crate) mod io`, so the validator can call `histogram_is_enough`;
     the continuation at line 2637 that lost its `\` (a user-facing refusal).
   - **`src/validate.rs`**: refuse a join over a `hist`-only `io.latency`,
     exit 2, which carries no `blk` (kernel).
   - **`src/main.rs`**: the `cost:` line naming the cpu before the window; the
     budget ladder's own `force:` reported as the user's pin; the mode ladder,
     after the decision (runner).
   - **`src/unwind.rs` with `tests/scenario_cpu.rs`**: a forced `--stack` mode
     this machine cannot serve from exit 3 to exit 1, in one commit
     (resolver).
   - **`src/bpf/mod.rs`**: `permission_advice` through
     `capabilities::withheld_file_capabilities()`, so the attach stops
     advising a grant already made (probes).
   - **`catalog/io.yaml`**: `keys:` on `io.bytes`'s `fs:proc` and `fs:cgroup`
     and `io.iops`'s `fs:proc`, the first shipped entry to reach
     `KeyNotCarried` (resolver).
   - **`src/collect/cpu/profile.rs`**: a user-only callchain still primes the
     kernel's symbols, most of the 968 ms before the window (runner);
     optionally, the factory taking `resolved.stack` (resolver).
   - **`src/collect/mem/mod.rs`, `src/bpf/attach.rs`**: drop `net_probe` now
     that `collect::net` is public (runner).
   - **`src/event.rs`, `src/join.rs`**: `iface` compared by its name alone, so
     a rename is a second key and `net.iface`'s `iface:lo/drops` cannot join
     `net.qdisc`'s `iface:lo`; `proc` has no producer (docs).
   - **`examples/bench/harness.rs`**: the guardrail through `power.throttle`,
     or the phase 9 sentence kept (probes; `DECISIONS.md`, "Open").
   - **Tests**: the shim check whose string never matches in
     `tests/family_mem.rs:58`, `tests/scenario_mem.rs:165` and
     `tests/cross_memory.rs:130` (runner); the continuations that lost their
     `\` in `src/perf.rs:3457`, `src/collect/cpu/counters.rs:1354,1401` and
     `tests/scenario_runtime.rs:240` (kernel).
   - **`.github/expected-skips.txt`**: the bpftool skip's new wording (kernel).
   - **Documents**: `J/s` in the outputs quoted by
     `examples/gpu-draw-in-watts.yml`, `examples/power-energy.yml` and
     `docs/scenarios/power.md`, now `W` (runner); `docs/install.md:208` and
     `docs/families/sched.md:57` still call 6.1 the floor (docs);
     `docs/scenarios/cpu.md`'s row for `cpu.cycles`'s two collectors (probes);
     `docs/scenarios/net.md` defect 7 and `docs/scenarios/sched.md`'s loss
     path, and flaky tally #3 closed in `docs/flaky.md` (kernel); the five
     `docs/open-requests.md` rows (8388, 8552, 8612, 8658, 8747) marked open
     without being re-read (docs); a `LICENSE` file.
   - **The flaky tally**: new this wave, each passed alone once --
     `scenario_sched::a_forced_switch_program_splits...` (at the merge too),
     `unwind::each_mode_copies_what_it_declares...` (twice in a row at load
     12 to 18), `unwind::a_stripped_build_is_named_from_debuginfod...`,
     `family_mem::the_working_set_is_what_was_touched...`,
     `scenario_proc::defect_1` and `defect_2`, `scenario_mem`'s cache tests
     under THP `always`, and `scenario_mem::both_huge_page_backends...` (#21,
     failed again alone once).
4. **Phase 12, programs nobody wrote for it** (`ROADMAP.md`): with phases 0
   to 11 done bar the edges above, the next wave is the first of `cases/`:
   six programs not written for this project (`sqlite3` stripped, CPython,
   `loganalyzer.c`, a compiler, an HTTP server under load, `make -j`), each
   with a cause known from the program's own account or injected from
   outside, and the triage's first suspect held to it, by `omn triage` and
   by `omn-mcp`, at `perf_event_paranoid` 2.

Below the milestones, and not scheduled:

- **CTF import** — ingest an LTTng-UST stream and merge it into the timeline.
  A consumer of one more format, not a source: `DECISIONS.md` boundary 6 says
  why LTTng is a peer rather than a backend. Low priority; nothing depends on
  it and no measurement here is worse without it.
- **Four decisions are open** (`DECISIONS.md`): an `overlap` rule's `span`
  that encloses a drained row's occurrences (41 184 of 41 639 reclaimed pages
  uncredited on the reclaim crossing today); what one series made of two
  modes means to an emit, and the budget's denominator in nanoseconds (both
  the runner's proposals); and whether the `power` sources are the bench's
  guardrail. `cpu.cycles` on `pmu:sw`, open since the fourteenth wave, is
  settled and served.
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

Everything this machine cannot run as uid 1000, collected from the six
handoffs, with the exact commands. Nothing else in the wave needed more than
the two file capabilities `make` grants (`sudo -n setcap
cap_bpf,cap_perfmon+ep target/debug/omn`, through the sudoers line in
`CLAUDE.md`, which works here: `getcap` shows `cap_perfmon,cap_bpf=ep` at
`fbe6f19`). The docs, engine and resolver streams need nothing.

1. **Phase 6: the 2 M IOPS overhead of `io.latency --emit hist`** (kernel;
   needs `fio`, and `null_blk`, a module load). The launched `fio` is the
   target, with `--thread`, so its eight jobs are one process's threads:

   ```console
   sudo modprobe null_blk queue_mode=2 irqmode=0 completion_nsec=0 nr_devices=1 \
       gb=4 bs=4096 hw_queue_depth=256 submit_queues=8
   sudo chmod a+r /dev/nullb0
   cd /home/moha/repos/omniscient && make && \
       cargo test --test scenario_io -- --ignored --exact \
       the_in_kernel_histogram_costs_a_two_million_iops_load_under_one_percent --nocapture
   sudo rmmod null_blk
   ```

   It closes phase 6's first criterion if the IOPS ratio holds within 1 %.
2. **A `nosuid` mount, end to end** (probes). A copy cannot keep
   `security.capability` unprivileged (it needs `CAP_SETFCAP`):

   ```console
   sudo mkdir -p /mnt/omn-nosuid
   sudo mount -t tmpfs -o nosuid tmpfs /mnt/omn-nosuid
   cp target/debug/omn /mnt/omn-nosuid/omn
   sudo setcap cap_bpf,cap_perfmon+ep /mnt/omn-nosuid/omn
   /mnt/omn-nosuid/omn list sched.offcpu | grep 'needs    bpf'
   # expected: "... The binary /mnt/omn-nosuid/omn carries CAP_BPF and CAP_PERFMON,
   #            but this process was not given them: the filesystem at /mnt/omn-nosuid
   #            is mounted nosuid ..."
   sudo umount /mnt/omn-nosuid
   ```

3. **The eBPF programs' time inside the target in `cost.json`** (runner,
   optional). Reported as unknown here, `kernel.bpf_stats_enabled` being 0 and
   `BPF_ENABLE_STATS` needing `CAP_SYS_ADMIN`:

   ```console
   sudo sysctl kernel.bpf_stats_enabled=1   # before the run; =0 afterwards
   ```

4. **An installed tree** (the fourteenth wave's, still not done):

   ```console
   make release && sudo make install PREFIX=/usr/local
   sudo setcap cap_bpf,cap_perfmon+ep /usr/local/bin/omn   # the line install prints
   /usr/local/bin/omn list                                  # 92 here, as target/debug/omn
   ```

5. **`dm-delay`, phase 4's last injection** (and phase 12's "a file on a
   `dm-delay` device"). No test is written for it yet; the device it needs,
   20 ms on every read and write of a loop device:

   ```console
   truncate -s 1G /tmp/omn-dm.img
   LOOP=$(sudo losetup --show -f /tmp/omn-dm.img)
   echo "0 $(sudo blockdev --getsz $LOOP) delay $LOOP 0 20" | sudo dmsetup create omn-delay
   # ... the test, against /dev/mapper/omn-delay ...
   sudo dmsetup remove omn-delay && sudo losetup -d $LOOP
   ```

Not privilege, but not for a shared machine either, and never started by a
stream: **the matrix sweep** ("What remains", item 1: it boots guests, alone
at the end of a wave), **the join hour** (`cargo test --release --test
join_engine -- --ignored an_hour_of_joins`), and **the bench's second table**
(`scripts/bench.sh`), each on a quiet machine.

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
