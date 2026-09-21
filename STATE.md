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
every source. Fifty-nine sources across nine families collect for real, over
eight mechanisms: procfs and sysfs, `perf_event_open` counting and sampling,
netlink, an `LD_PRELOAD` shim, NVML through `dlopen`, `af_packet` with a
`PACKET_MMAP` ring, and eBPF through raw tracepoints, classic tracepoints,
kprobes and uprobes. Every emit the catalog declares exists, all nine output
formats have an encoder, all four join match kinds work, stacks are captured
two ways — a `perf_event_open` callchain for `cpu.profile` and
`bpf_get_stackid` for `sched.offcpu`, which is the one that says *where* a task
blocked — and a run measures and reports what it cost, against a ceiling it can
be held to.

**1040 tests pass**, of which 175 are scenarios: a workload whose property the
scenario chose, a session that measures it, and a check against that property.
They found 63 defects, none worked around, recorded in `docs/scenarios/`; the
fifth wave closed 52 of them, and `docs/fixes/` records what became of each.

What is missing is breadth, not skeleton. Adding a source is additive; the
expensive parts are done.

---

## What the numbers say

```
100 sources: 86 usable here, 10 unverified, 4 out of reach on this machine.
59 of them have a collector, covering 67 of their 202 backends.
```

Three different questions, deliberately kept apart.

| Question | Answer | Meaning |
|---|---|---|
| What can this machine serve? | 86 | probed now: the file is readable, the event opens, the tracepoint is there |
| What can this build collect? | 59 sources, 67 backends | a collector is written and registered |
| What is neither? | the rest | vocabulary: it resolves, it validates, and it says so |

That `86` is with `CAP_BPF` and `CAP_PERFMON` on the binary, and it is why
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
and finding 36 records what that moved. `86` is what this machine can serve
through a path that attaches, checked name by name against `/sys/kernel/btf/
vmlinux`.

It cuts the other way too, which cost a session to find: a binary that carries
capabilities is non-dumpable, so its own `/proc` entries belong to root and the
three `mem.heap.*` sources lost the `pagemap` they read. Finding 14 of
`docs/open-findings.md` has it; `main.rs` now restores dumpability at startup.

Per family, sources with a collector:

| Family | With a collector | Sources | Usable here |
|---|---|---|---|
| `mem` | 16 | 23 | 22 |
| `cpu` | 13 | 14 | 12 |
| `net` | 7 | 15 | 12 |
| `gpu` | 5 | 9 | 6 |
| `io` | 5 | 14 | 13 |
| `power` | 5 | 5 | 4 |
| `sched` | 5 | 10 | 9 |
| `proc` | 2 | 6 | 6 |
| `runtime` | 1 | 4 | 2 |

The gap between the two rightmost columns is now the project's whole remaining
breadth, and it is collectors rather than mechanism: `io` can serve thirteen
sources and collects five, `proc` can serve six and collects two.

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
| 4 — the vertical case | **partly**. Two real correlations exist and are checked, one per match kind that has a producer. The HTTP-request decomposition it describes needs the eBPF backends of `io` and `net`. |
| 5 — generalizing the join engine | **done for the engine**. All four match kinds — `interval`, `eq`, `scoped_eq`, `wakeup` — are implemented and each reports in its own words. What is missing is producers: `scoped_eq` needs `blk:rq@open_ts`, which needs the `io` eBPF backends. |
| 6 — emit, aggregation, budget | **partly**. Every emit exists, and every declared output format has an encoder — `folded`, `speedscope`, `pprof` and `chrome` included. A run measures its own cpu against the target's and is held to `--budget`, degrading or stopping and saying which. What is left is the fourth task: **hot attach and detach**, which is also what `DECISIONS.md` defers concurrent requests on, and what phase 10's escalation assumes. |
| 7 — declarative configuration | **done**. Sessions in YAML, JSON and TOML; a dumped request reads back unchanged; the resolved file is written beside the results. |
| 8 — self-benchmark | **partly**. The cost of a run is measured and written beside the results as `cost.json`. What is left is running one measurement through every backend of a chain and comparing what each cost — `--backend force:` does the pinning, and the scenario wave uses it for agreement rather than for price. |
| 9 — breadth | in progress; 59 of 100 sources, 67 of 175 backends. The 41 without a collector are named below, and 29 of them are reachable by a mechanism this build already has. |
| 10 — intelligence layer | not started. |

---

## What remains, in the order it is worth doing

Forty-one sources have no collector, and **all but eight of them are blocked by
nothing at all**: the catalog pass below has landed, so what is left for them is
a collector written against an attach path that works today. The fifth wave
closed 52 of the sixty-three defects the scenario wave found; the eleven that
are left are named in `docs/fixes/`.

### 1. ~~The cheapest lever: the catalog entries~~ — **done**

Twenty-seven entries gained a `bpf:raw_tp` ahead of their `bpf:tp`, and the two
probes finding 15 asked for are wired. `86 usable here` instead of `62`, `4 out
of reach` instead of `22`, and twenty-seven sources that answered *"this machine
cannot serve it"* now answer *"no collector is written for it yet"*. Finding 36
of `docs/open-findings.md` records what was checked and what was left.

Three entries were deliberately not converted: `gpu.job`, whose tracepoints are
in a module and absent from `vmlinux` BTF, and `io.fs` and `io.readahead`, whose
requirement is a glob rather than a name — converting those means choosing which
tracepoint the collector attaches to, which belongs to whoever writes it.

### 2. The collectors that nothing blocks — 24 sources

`src/bpf/attach.rs` has raw tracepoints, classic tracepoints, kprobes and
uprobes, the raw path reaches every one of these, and since item 1 the catalog
says so. `BRIEF_NEXT.md` divides them by family.

**`io.latency` and `io.queue` are the two that matter most**: they are the only
sources that would produce `blk:rq@open_ts`, and `scoped_eq` is the one join
match kind the engine implements that **nothing produces**. Landing them turns a
quarter of the join engine from theory into something checked, and it is what
phase 4's I/O path waits on.

What is still genuinely missing as mechanism is **fentry** (BTF-typed attach,
`BPF_PROG_TYPE_TRACING` with an `attach_btf_id`) for `io.syscall`, `io.fs`,
`io.fsync`, `net.tcp.connect` and `net.socket`, and **USDT** (the target's ELF
notes, then a uprobe) for the three `runtime` sources.

### 2c. The distribution that does not say it is short — finding 37

New, and it is a defect rather than breadth. A `hist` or a percentile built from
a run that dropped or was throttled is rendered exactly like one built from a
complete run, and the events it is missing are the slow ones — whatever stalled
the machine is what stopped the measuring. So every percentile from a lossy run
is a lower bound and nothing says so.

The largest opening was already closed by construction and for a better reason:
`poll` cannot produce `hist` at all, because a histogram of a polled gauge is a
histogram of when the poller looked. What is left is `sample` and `trace`, whose
losses *are* counted — the invariant holds — and whose distributions do not
carry the fact. `src/emit.rs` and `src/render.rs`. It should land before phase
10, because a triage layer reading a p99 to decide what to escalate is exactly
who a short tail misleads.

### 2b. The eleven defects the fifth wave left

`docs/fixes/` names each with why it was left: two need a line in a file the
stream did not own, three are decisions rather than tasks, and one — `topk`
ranking a series rather than an attribute — needs surface syntax nobody owns.

### 3. The sources nothing mechanical blocks — 6 sources

`mem.page_alloc` and `mem.kmalloc` want the preload shim they already have;
`mem.sharing` and `mem.latency` want PEBS or IBS; `mem.bandwidth` wants uncore
PMUs; `cpu.flow` wants Intel PT. Each is independent of the others.

### 4. Phase two of the scenarios — the crossings

Written up in `BRIEF_NEXT.md` and not dispatched: four questions a single family
cannot answer. `heap.alloc → mem.faults → mem.rss` for memory end to end;
`cpu.profile` beside `sched.offcpu` for where the time went; the I/O path once
`blk:rq@open_ts` has a producer; and the machine's own state as a confounder.
Phase one left the workloads and the keys for them.

### 5. The smaller ones, each with its reason

- **`Unit::Watts` does not exist**, which is part of why `gpu.power` reports an
  energy counter.
- **The GPU is not in the environment capture**, and two runs are comparable
  only if they were taken on the same device under the same driver.
- **`net.capture` needs `CAP_NET_RAW`**, which the build does not grant. That is
  a decision rather than an omission: the capability lets the binary read every
  frame on the machine, and the scenario wave deliberately left it to be made
  rather than making it.
- **The closing read of a polled counter.** `net` takes one and `io`/`power`
  deliberately do not, for a reason each. Both cannot be right.

---

## How this parallelizes

Four waves of parallel agents have run on this project, and each was divided
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

The sixth wave divides by family again, plus one layer stream: five streams
writing eBPF collectors into one family directory each, and one rewriting
`src/emit.rs` and `src/render.rs` for finding 37, which shares no file with any
of them. `BRIEF.md` is that brief and `scripts/spawn.sh` is wired for it.

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
