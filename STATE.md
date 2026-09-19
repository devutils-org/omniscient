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
every source. Forty-four sources across nine families collect for real, over six
mechanisms: procfs and sysfs, `perf_event_open` counting and sampling, netlink,
an `LD_PRELOAD` shim, NVML through `dlopen`, and eBPF raw tracepoints. Every
emit the catalog declares exists — `raw`, `timeline`, `summary`, `rate`,
`hist`, `topk` and `stack` — and stacks are captured two ways: from a
`perf_event_open` callchain for `cpu.profile`, and in the kernel with
`bpf_get_stackid` for `sched.offcpu`, which is the one that says *where* a task
blocked. All four join match kinds work, and every filter field the catalog
declares is now on an event or refused by name. **643 tests pass**, and the
suite is the project's main asset — several of its tests have found real
defects in the code they were written against.

What is missing is breadth, not skeleton. Adding a source is additive; the
expensive parts are done.

---

## What the numbers say

```
100 sources: 80 usable here, 15 unverified, 5 out of reach on this machine.
44 of them have a collector, covering 48 of their 165 backends.
```

Three different questions, deliberately kept apart.

| Question | Answer | Meaning |
|---|---|---|
| What can this machine serve? | 80 | probed now: the file is readable, the event opens, the tracepoint is there |
| What can this build collect? | 44 sources, 48 backends | a collector is written and registered |
| What is neither? | the rest | vocabulary: it resolves, it validates, and it says so |

That `80` is with `CAP_BPF`, `CAP_PERFMON` and readable tracefs. The same
binary without them reports `53`: the difference is the whole eBPF and per-cpu
perf surface, and it is why `make` reapplies the capabilities that every relink
drops. Nothing announces the loss — the tool reports fewer sources, which reads
like the machine changed rather than the binary.

It cuts the other way too, which cost a session to find: a binary that carries
capabilities is non-dumpable, so its own `/proc` entries belong to root and the
three `mem.heap.*` sources lost the `pagemap` they read. Finding 14 of
`docs/open-findings.md` has it; `main.rs` now restores dumpability at startup.

Per family, sources with a collector:

| Family | With a collector | Sources | Usable here |
|---|---|---|---|
| `cpu` | 11 | 14 | 12 |
| `mem` | 7 | 23 | 20 |
| `gpu` | 5 | 9 | 6 |
| `io` | 5 | 14 | 11 |
| `net` | 5 | 15 | 10 |
| `power` | 5 | 5 | 4 |
| `sched` | 3 | 10 | 9 |
| `proc` | 2 | 6 | 6 |
| `runtime` | 1 | 4 | 1 |

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
| 6 — emit, aggregation, budget | **partly**. Every emit exists: `raw`, `timeline`, `summary`, `rate`, `hist`, `topk` and `stack`, with `folded` and `speedscope` encoders behind the last. `--budget` is still parsed and not enforced. |
| 7 — declarative configuration | **done**. Sessions in YAML, JSON and TOML; a dumped request reads back unchanged; the resolved file is written beside the results. |
| 8 — self-benchmark | **not started**. `--backend force:` exists for it. |
| 9 — breadth | in progress; 44 of 100. The 56 without a collector are named below, and 27 of them wait on one thing. |
| 10 — intelligence layer | not started. |

---

## What remains, in the order it is worth doing

Fifty-six sources have no collector. They are not fifty-six problems: **27 of
them wait on one piece of work**, and the rest fall into a handful of groups
that share nothing and can be built in any order.

### 1. eBPF beyond raw tracepoints — 27 sources

The single largest lever in the project. `src/bpf/` attaches exactly one way,
`BPF_RAW_TRACEPOINT_OPEN`, which is why `sched.offcpu` and `sched.wakeup` work
and nothing else does. What the catalog asks for beyond it:

| Attach | Sources | What it needs |
|---|---|---|
| `bpf:tp` | 27 | a numeric id out of tracefs and `PERF_EVENT_IOC_SET_BPF`, or the entry moved to `raw_tp` where the events are the same |
| `bpf:fentry` | 5 | BTF-typed attach, `BPF_PROG_TYPE_TRACING` |
| `bpf:usdt` | 3 | reading the ELF notes of the target, then a uprobe |
| `bpf:kprobe`, `bpf:uprobe` | fallbacks | `perf_event_open` with `PERF_TYPE_KPROBE` |

A second, cheaper half of the same lever: several `bpf:tp` entries name events
that `raw_tp` can attach to just as well, and the entry orders them the way the
project settled — strictly fewer requirements, same events. Moving those is a
catalog change with a collector behind it, not a new mechanism.

`docs/bpf-layer.md` has the calling conventions, and `offcpu.rs` is the worked
example of in-kernel aggregation with a stack map.

### 2. The sources that need no privileges at all — 7 sources

`mem.pressure`, `mem.swap`, `mem.thp`, `mem.numa`, `io.fs`, `proc.*` partly, and
`power`'s remaining sysfs readers. Each is a procfs or sysfs file with a shape
of its own, each is worth a day, and none of them touches anything another one
touches. This is the cheapest breadth in the project and it is being left
behind because eBPF is more interesting.

### 3. The counting sources that need per-cpu perf — 5 sources

`cpu.topdown`, `mem.cache_*`, `mem.bandwidth`: `perf_event_open` over a cpu
rather than a task, which `src/perf.rs` has as `Scope::Cpu` and no collector
uses. `mem.bandwidth` additionally needs uncore PMUs, which are a different
sysfs layout and a different privilege story.

### 4. `pprof` and `chrome` encoders

Declared, refused by name, and now unblocked: `folded` and `speedscope` both
work, so the stack side has a producer and a shape to follow. `pprof` is a
gzipped protobuf with its own string table; `chrome` is a timeline encoder and
needs no stacks at all.

### 5. `--budget` is parsed, explained, and not enforced

It already says what it would do — which backend it would have chosen, and
whether the selected one is within the ceiling. Enforcing it means measuring
the collector's own cost during a run, which is phase 8's self-benchmark, and
degrading `trace` to an in-kernel `hist` where the budget is exceeded.

### 6. The smaller ones, each with its reason

- **`Unit::Watts` does not exist**, which is part of why `gpu.power` reports an
  energy counter.
- **The GPU is not in the environment capture**, and two runs are comparable
  only if they were taken on the same device under the same driver.
- **An exit code for "valid, but this mechanism is not built"** — it returns 1,
  like a real failure; `NoCollector` returns 3 and this belongs with it.
- **The closing read of a polled counter.** `net` takes one and `io`/`power`
  deliberately do not, for a reason each. Both cannot be right, and a reader
  comparing two families gets a different convention in each.
- **`power.cstate` declares `state` as a filter field and cannot carry it**:
  one event covers every idle state of a cpu. The eBPF backend, which sees
  transitions one at a time, is where that field becomes real.

---

## How this parallelizes

The groups above were chosen so that they do. What they own, and where they
collide:

| Stream | Owns | Touches nothing else in | Blocked by |
|---|---|---|---|
| **A — eBPF attach kinds** | `src/bpf/*`, `docs/bpf-layer.md` | — | nothing |
| **B — procfs/sysfs breadth** | `src/collect/{mem,io,proc,power}/` new files | shares only `catalog/*.yaml` | nothing |
| **C — per-cpu perf** | `src/perf.rs`, `src/collect/cpu/`, `src/collect/mem/cache*` | — | nothing |
| **D — encoders** | `src/encode.rs`, `src/format.rs` | — | nothing |
| **E — budget and self-benchmark** | `src/collect/mod.rs` runner, `src/main.rs` | — | nothing to start; needs C to have something expensive to measure |

A and B are the two that matter and they are independent: A builds mechanism,
B builds readers. C is independent of both. D touches one file nobody else
does. E is the only one with an ordering constraint, and only a soft one.

**The one shared file is `catalog/*.yaml`**, which every stream edits — each
adding backends or caveats to its own sources. One file per family keeps that
to an append per stream, which is the cheapest kind of conflict.

**The one shared seam is `src/collect/mod.rs`**, where families register. It is
a list of `Factory::new` calls per family module, deliberately so: nine agents
editing one `match` arm is nine conflicts on the same three lines.

So: **five streams, four of them fully parallel.** The honest ordering, if
fewer hands are available, is A then B — A unlocks 27 sources and everything in
phase 4's vertical case, B unlocks 7 for a fraction of the effort.

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
