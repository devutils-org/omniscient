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
it cost, against a ceiling it can be held to.

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

**1830 tests pass** after the eleventh wave's merge, eight more ignored, and
four failed in that run: three passed alone at once (load-sensitive, in the
flaky tally) and one is `cross_compute`'s lossy profile, below. The ignored are
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
101 sources: 93 usable here, 4 unverified, 4 out of reach on this machine.
96 of them have a collector, covering 106 of their 205 backends.
```

Three different questions, deliberately kept apart.

| Question | Answer | Meaning |
|---|---|---|
| What can this machine serve? | 90 | probed now: the file is readable, the event opens, the tracepoint is there |
| What can this build collect? | 96 sources, 106 backends | a collector is written and registered |
| What is neither? | the rest | vocabulary: it resolves, it validates, and it says so |

That `90` is with `CAP_BPF` and `CAP_PERFMON` on the binary, and it is why
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

It cuts the other way too, which cost a session to find: a binary that carries
capabilities is non-dumpable, so its own `/proc` entries belong to root and the
three `mem.heap.*` sources lost the `pagemap` they read. Finding 14 of
`docs/open-findings.md` has it; `main.rs` now restores dumpability at startup.

Per family, sources with a collector:

| Family | With a collector | Sources | Usable here |
|---|---|---|---|
| `mem` | 22 | 23 | 22 |
| `net` | 15 | 16 | 13 |
| `io` | 14 | 14 | 14 |
| `cpu` | 13 | 14 | 12 |
| `sched` | 10 | 10 | 9 |
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
| `mem.faults` | `perf_event_open`, sampling | against the kernel's own minor-fault counter for the same process |
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
| `sched.switches` | `perf_event_open` | against the kernel's own switch counter for the same thread |
| `sched.offcpu` | eBPF raw tracepoint | a thread that sleeps a known 300 ms, whose off-cpu duration and interruptible state must both match |
| `cpu.profile` | `perf_event_open`, sampling with `PERF_SAMPLE_CALLCHAIN` | a cpu-bound loop in the test binary, whose own symbols must appear in the table |
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
| 0 — decisions | **done**. Language, clock, scope, kernel floor 6.1, `CAP_BPF`+`CAP_PERFMON`, raw tracepoints first. |
| 1 — event model and request schema | **done**. `Event`, `Span`-equivalent attributions, `Request`, `ResolvedRequest`, typed keys, one clock domain. |
| 2 — catalog, resolver, CLI | **done**. 100 entries as data, machine introspection, a resolver that records why each backend was passed over, a validator that refuses before attaching. |
| 3 — first collectors and transport | **done**. Ring buffers for perf and eBPF, loss accounting including throttling, a launch gate, frame-pointer unwinding and symbolization through the target's own maps. |
| 4 — the vertical case | **done, except `dm-delay`, which needs root.** One request decomposed boundary by boundary on a real server, the lock's holder and the block device named, and since the tenth wave the tree's two network rows: a client that pauses 30 ms mid-request puts 30.14 to 30.27 ms in `net_rx`, and a slow reader puts 32.8 to 33.8 ms in `net_tx`, both injected from the client without root. The rows sum to the client's latency within +0.06 % to +0.36 % (+3.1 % for the slow reader, whose last buffer the client reads after the server is done). The network rows print `IDENTITY` where they are `KEY`; the engine change is filed. |
| 5 — generalizing the join engine | **done**, and a fifth kind since the eighth wave. `interval`, `eq`, `scoped_eq` and `wakeup` each have a producer; `between` is the one three crossings asked for independently, and it is `TEMPORAL` by construction — a polled total's change credited to what moved with it, which the ladder must not call `KEY`. Beside the joins, a `decompose:` block with a signed residue that is never rounded away. |
| 6 — emit, aggregation, budget | **partly**. Every emit exists, and every declared output format has an encoder — `folded`, `speedscope`, `pprof` and `chrome` included. A run measures its own cpu against the target's and is held to `--budget`, degrading or stopping and saying which. And since the tenth wave, **hot attach**: a request carries a `window` inside the run, attached and detached while the rest keep running, and two sources attached for one phase each of a program count that phase within +0.17 % and nothing of the other. Three edges are filed with the files that own them: the join engine credits events after a detach to a stale state, the results document does not yet say where a window was, and a per-task counter attached late to a program that already has threads counts one of them. |
| 7 — declarative configuration | **done**. Sessions in YAML, JSON and TOML; a dumped request reads back unchanged; the resolved file is written beside the results. |
| 8 — self-benchmark | **done.** The harness has every task the phase lists, and it has measured: 94 (source, backend) pairs alone on this machine, every workload admitted by a gate that judges what a median resolves and refuses a busy machine (`docs/bench.md`). The resolver's seven choices between served backends were confirmed. What the table corrects is the cost bands: an in-kernel collector's cost is in the target's dilation, not the tool's cpu (`proc.syscall`: 0.42 % against +296 %), and the preload shim cost an allocation-bound program about 150 times -- two system calls a record -- until it was moved onto a shared ring, which brought a record to about 70 ns. Finding 47 files each. |
| 9 — breadth | nearly done; **96 of 101 sources, 106 of 205 backends**. The five without a collector wait on hardware this machine lacks or on a vendor library. |
| 10 — intelligence layer | **its first method is done by the engine**. TSA — where a thread's wall clock went — is what `examples/join-decompose-compute.yml` produces: three states, each row's kind and resolution, the program's life as the whole, and a residue inside its own resolution. Since the tenth wave the layer above it exists and calls the engine like any other user: the objective is required and refused when missing, `omn latency` names what dominates a run or its tail, `omn saturate` reads a queue by Little's law, and `omn diff` subtracts two profiles -- each checked against a workload whose limit the test chose. Since the eleventh wave **the escalation** closes it: a session's `escalate:` block attaches the layer the cheap pass names mid-run, detaches the rest and holds the budget; a program cpu-bound then lock-bound gets the lock layer 0.4 to 0.5 s after the switch (`docs/triage.md`). **Done.** |
| 11 — a model reads it | **done** (the twelfth wave): `omn-mcp`, an MCP server over stdio in the workspace, exposing the catalog, the runs a directory holds and this machine's sweep, as the documents themselves regrouped per request with what each is missing stated first. It probes nothing (the `omn` binary answers every machine question), and runs a session only when its operator allowed it, bounded, budgeted, and never one that launches a program (`docs/mcp.md`). What it filed: `omn list --format json` carrying the sweep, so the server stops parsing the text table. |

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

**The fourteenth wave's brief is written** (`BRIEF.md`), not yet dispatched:
the stock machine (P), the results' vocabulary and renderings (R), the kernel
programs (K), an agent benchmarking a program through `omn-mcp` (J), and a
release that installs (D). The CI logs it reproduces from are kept in
`../omniscient-ci-logs/`, outside the repository.

**After the thirteenth wave, in order:**

1. **A product wave.** Every roadmap phase is done but phase 6's three filed
   edges, phase 9's last five sources and phase 4's `dm-delay`; what is missing
   is what a user who is not the developer would need: the CI green on
   a push -- its second run (`docs/open-findings.md`, "The CI's second run")
   found the two defects to fix first: `cpu.profile` refused at the kernel's
   default `perf_event_paranoid` 2, which takes every method built on a
   profile with it, and `cpu.cycles` read available on a cpu with no hardware
   PMU -- a cell at a
   distribution's defaults (Ubuntu's `perf_event_paranoid` 4, where every perf
   test must skip rather than fail), a release that installs, and one use case
   polished end to end -- an agent benchmarking a program through `omn-mcp`.
2. **The requests the thirteenth wave filed**, under its headings in
   `docs/open-findings.md`: per-ring drops in `cpu.profile`, `KeyKind::Node`,
   `Unit::Watts` and `KeyNotCarried` (each an enum arm R's matches must take),
   a container resolved to its cgroup, a window that closes at each collector's
   last read, and the read-ahead scenario's one lookup in 256. Refresh
   `docs/open-requests.md` first: it is a snapshot from before the twelfth wave.
3. **The bench's second table**, on a quiet machine: the bands changed.
4. **`dm-delay`**, the one injection left that needs root.

Below the milestones, and not scheduled:

- **CTF import** — ingest an LTTng-UST stream and merge it into the timeline.
  A consumer of one more format, not a source: `DECISIONS.md` boundary 6 says
  why LTTng is a peer rather than a backend. Low priority; nothing depends on
  it and no measurement here is worse without it.
- **One decision is open** (`DECISIONS.md`): whether `sched.switches` gets a
  `sched_switch` program that serves every target, a cgroup included.

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

- **`Unit::Watts` does not exist**, which is part of why `gpu.power` reports an
  energy counter.
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
