# Omniscient — ROADMAP

Implementation roadmap. Meant to be executed by an agent, step by step,
**without skipping a milestone**.

Related documents: `CATALOG.md` (the vocabulary of sources).

---

## Guiding principle

The ordering is not "simplest to hardest". It is **"what is expensive to change
later"**.

A collector can be rewritten in a day. A change to the event model or to the
clock domain has to be paid for across every existing collector. So: model first,
skeleton next, breadth last.

**Milestone exit rule**: every phase has measurable criteria. Until they are met,
do not start the next one. A milestone that is "almost done" and gets paired with
a new worksite never finishes.

---

## Standing anti-goals

To be recalled at every phase — these are the project's natural drifts:

- ❌ **Do not wrap `perf`, `bpftrace` or `bcc`.** Omniscient talks directly to
  `perf_event_open`, `bpf()`, procfs/sysfs, netlink. Shelling out means the
  project failed.
- ❌ **Do not implement breadth-first.** Seven half-finished collectors and zero
  correlation is the default outcome if you follow the catalog in order.
- ❌ **Do not hard-code source availability.** Everything goes through machine
  introspection.
- ❌ **Do not degrade silently.** An unavailable source, a filter that was not
  pushed down, a fallback backend, lost events: all of it must surface.
- ❌ **Do not chase bit-exact determinism.** That lives outside the kernel-level
  core (it is dynamic binary translation). An accepted boundary, to be documented.
- ❌ **Do not promise the inside of NVIDIA GPUs.** CUPTI is closed, there is no
  kernel path. An accepted boundary.
- ❌ **Do not tune anything.** No `madvise`, no `sched_setaffinity`, no queueing
  discipline, no cgroup limit, and nothing that modifies the target in order to
  read it. A tool that measures and changes in the same run produces numbers
  nobody can interpret. An accepted boundary, and the one most likely to be
  proposed as a feature.

---

## Phase 0 — Decisions to lock down

**No code.** These choices constrain everything else.

| Decision | Options | Criterion |
|---|---|---|
| Language | Rust + libbpf-rs · C + libbpf · Go + cilium/ebpf | CO-RE mandatory; Rust if the correlator is large |
| Kernel floor | e.g. 5.15 (BTF, ringbuf) or 6.1 (`lock:contention_*`) | determines which fallbacks must be written |
| Clock | `CLOCK_MONOTONIC` **everywhere** | non-negotiable, see Phase 1 |
| Privileges | root · `CAP_BPF`+`CAP_PERFMON` · unprivileged degraded | changes half the backends |
| Scope | single machine (phases 1-8) · fleet (later) | multi-host adds inter-machine clock drift |
| Usage target | one-off session · continuous daemon · daemon with a flight recorder | sampling vs capture, online aggregation vs raw storage. The third is the second plus a trigger, and it is what catches a p99.9 that cannot be reproduced. Settled in the ninth wave: a session, with hot attach and the flight recorder inside it and no daemon (`DECISIONS.md`) |

**Output**: a filled-in `DECISIONS.md`. Every line settled, with the reason.

---

## Phase 1 — Event model and request schema

The only genuinely expensive deliverable to get wrong. To be done on paper before
any code.

### Tasks

1. **`Event` struct**: `ts_ns`, `ts_err_ns`, `kind`, `keys`, `attrs`,
   `collector_id`, `seq`. Append-only, never mutated.
2. **Typed keys**: `task:tid`, `proc:pidns+pid`, `file:dev+ino`, `sock:cookie`,
   `blk:rq@open_ts`, `mem:[base,len)`, `gpu:fence_ctx+seqno`, `req:<id>`,
   `cgroup:id`. Only three match families: **equality**, **scoped equality**
   (pointer + open timestamp), **interval**.
3. **`Span` struct**: `id`, `parent_id`, `start/end`, `keys`, `rule_id`,
   `evidence` ∈ {`IDENTITY`, `KEY`, `WAKEUP`, `TEMPORAL`}.
4. **`Request` struct**: `source`, `target`, `mode`, `emit`, `keys`, `filter`,
   `budget`. A single object built by the CLI, the YAML file, or the API.
5. **`ResolvedRequest` struct**: the request plus the selected backend, pushed-down
   vs userspace filters, machine capabilities, degradations. This is the
   "lockfile".
6. **Clock policy**: `CLOCK_MONOTONIC` as the only domain. On the
   `perf_event_open` side, set `use_clockid` + `clockid` explicitly. On the eBPF
   side, **`bpf_ktime_get_ns()` only** — `bpf_ktime_get_boot_ns()` includes
   suspend time; mixing the two is a silent bug that destroys every join. Device
   domains (GPU, NIC): explicit calibration + `ts_err_ns`.

### Exit criteria

- [ ] The five structs are written and documented.
- [ ] A unit test proves that a serialized/deserialized `Event` is identical.
- [ ] A one-page note justifies each key: which boundary it crosses.

---

## Phase 2 — Catalog, resolver, CLI skeleton

Still **no collector**. We build the machinery that will host them.

### Tasks

1. **Catalog loader**: parse the YAML entries (catalog schema §1).
2. **Machine introspection**: read `/sys/kernel/tracing/events`,
   `/sys/bus/event_source/devices`, kernel BTF, `perf_event_paranoid`, BPF
   capabilities, kernel version. Produces a `Capabilities`.
3. **Resolver**: `(source, mode, capabilities, scale) → backend`. Walks the
   backend chain, keeps the first viable one, records the degradations.
4. **Validator**: rejects invalid combinations **before any attachment**, with a
   message that says why (e.g. `mem.bandwidth --pid` → uncore is inherently
   system-wide).
5. **CLI**: `omn <family>.<metric> [options] [target]`, plus `omn list` and
   `omn --dump-request`. The CLI **builds a `Request`**, it executes nothing.
6. **`omn list`**: a catalog projection **filtered by actual availability**. Not a
   parallel list to maintain.

### Exit criteria

- [ ] `omn list` shows different sources on two different machines.
- [ ] `omn --dump-request io.latency --pid 1` prints a complete `ResolvedRequest`.
- [ ] Three invalid combinations fail with three distinct, clear messages.
- [ ] No collector exists yet. **If collection code was written here, that is a
      sequencing error.**

---

## Phase 3 — First collector and transport plumbing

Two collectors only, chosen to exercise the entire plumbing.

### Tasks

1. **`sched.wakeup`** (`bpf:tp` sched_wakeup) — carries the wakeup chain, a
   dependency of every other family.
2. **`sched.offcpu`** (`bpf:tp` sched_switch + stacks) — exercises in-kernel
   aggregation.
3. **Transport**: `BPF_MAP_TYPE_RINGBUF`, userspace consumer, decoding into
   `Event`.
4. **Loss accounting**: a dropped-event counter, surfaced **all the way to the
   user-visible output**. A collection that loses 3 % without saying so produces
   wrong numbers that look right.
5. **Reordering**: ring buffers are per-CPU, events arrive unsorted. Bounded
   window + **watermark**: finalize nothing until every collector has passed T.
   This assumes a minimum pipeline latency.
6. **Symbolization**: frame pointers first. DWARF/ORC later (blazesym or libdw).
   Do not write your own DWARF unwinder.

### Exit criteria

- [ ] `omn sched.offcpu --pid X --emit=hist` produces a correct histogram.
- [ ] The loss rate is displayed, and it is zero under moderate load.
- [ ] A test with a program that sleeps N ms measures N ms ± 5 %.
- [ ] Reordering is validated by injecting out-of-order events.

---

## Phase 4 — The vertical case (the real milestone)

**An HTTP request that reads a file and sends it back.** It crosses network RX,
scheduling, CPU, synchronization, VFS, page cache, block, device, network TX.

### Boundaries to implement

| # | Boundary | Upstream → downstream key | Mechanism | Evidence |
|---|---|---|---|---|
| 1 | NIC → softirq → TCP | skb → `sock:cookie` | `bpf_get_socket_cookie` | KEY |
| 2 | socket ready → thread woken | `sock:cookie` → `task:tid` | `sched_wakeup` | WAKEUP |
| 3 | request entry | `task:tid` → `req:id` | `bpf_task_storage` | IDENTITY |
| 4 | lock wait | `task:tid` → holder | `sched_switch` + `wakeup` | WAKEUP |
| 5 | `read()` → cache miss | fd → `file:dev+ino` | fd resolution at entry | KEY |
| 6 | FS → block submission | → `blk:rq@ts` | **re-read `task_storage` at `block_rq_issue`** | IDENTITY |
| 7 | submit → completion | `blk:rq@ts` | scoped equality + TTL | KEY |
| 8 | completion → wakeup | rq → `task:tid` | `sched_wakeup` from IRQ | WAKEUP |
| 9 | response | `req:id` → `sock:cookie` | `task_storage` | IDENTITY |
| 10 | TCP TX → NIC | `sock:cookie` → PHC | `SO_TIMESTAMPING` | KEY |

**Step 6 is the crux.** Do not join `bio → inode → file → fd → task` (fragile, 4
inferred joins). Read the `req:id` from the current task's storage at
`block_rq_issue`: a read is submitted in the reader's context. One propagated key
replaces four inferences.

### Deliverable

```
req:a3f1   total 412 ms
├── net_rx              0.4 ms   KEY
├── sched_wait          8.1 ms   WAKEUP
├── cpu_on             12.3 ms   IDENTITY
├── lock_wait          41.2 ms   WAKEUP     → holder tid=8821
├── block_io          347.0 ms   IDENTITY   → 3 rq, dev 259:0
└── net_tx              2.8 ms   KEY
```

### Exit criteria

- [x] The latency budget decomposes on a real server, not a mock. *A TCP
      server on loopback, `examples/vertical-one-request.yml`, ninth wave.*
- [x] Every line carries its evidence level. *`IDENTITY` for every row read by
      the request id the serving thread carried.*
- [x] The sum of the segments ≈ the client-side measured latency (gap < 10 %).
      *+0.11 % to +0.42 % over six runs, one of them under load; positive every
      time, as it has to be — the loopback and the parse are the client's.*
- [~] **Ground truth**: a synthetic server with known injected delays (`tc netem`,
      `dm-delay`, `usleep` under a lock) is decomposed correctly. Without that
      test, nothing proves the correlation is right rather than merely plausible.
      *`usleep` under a lock and a cold read, each landing in its own row. `tc
      netem` and `dm-delay` need root, and are the two injections left — which
      is also why the tree's `net_rx` and `net_tx` rows are not decomposed yet.*

---

## Phase 5 — Generalizing the join engine

Now, and not before: we have three real join types to generalize from.

### Tasks

1. **Declarative rules**: `{from, to, via, window, match}` where `match` ∈
   {`eq`, `scoped_eq`, `interval`, `wakeup`}. The engine knows nothing about `bio`
   or `malloc`, it applies rules.
2. **Join placement**: kernel (eBPF map) / agent (stream) / query (storage).
   **Decided per rule, not globally.** The model must allow moving a join from one
   tier to another without rewriting the collector.
3. **Open-state lifecycle**: TTL, bounded map size, **eviction counters
   surfaced**. Delete on completion, not only on expiry.
4. **Scoped equality**: `(pointer, open_ts)` everywhere a kernel pointer is used as
   a key. Pointers get recycled.

### Exit criteria

- [ ] Adding a boundary means adding a rule, zero lines of engine.
- [ ] A load test proves there is no open-state leak over 1 h.
- [ ] Evictions appear in the user-visible output.

---

## Phase 6 — Emit, aggregation, budget

### Tasks

1. **Emits**: `raw`, `hist` (log2 and linear), `rate`, `topk`, `stack`, `timeline`.
2. **In-kernel aggregation**: histograms and top-k computed inside the eBPF
   program. That is what makes the overhead viable — two million events become a
   few hundred bytes.
3. **`--budget N%`**: measure the collector's actual cost (consumer CPU, loss
   rate), degrade automatically (`trace` → in-kernel `hist` → `sample`), **and say
   what was degraded**.
4. **Hot attach/detach**, without restarting the agent or breaking in-flight
   joins. A constraint to set now: it conditions the escalation in phase 10.

### Exit criteria

- [ ] `io.latency --emit=hist` at 2 M IOPS stays under 1 % overhead.
- [ ] Exceeding the budget produces an **announced** degradation, not a loss.
- [ ] A collector can be added and removed mid-session.

---

## Phase 7 — Declarative configuration

### Tasks

1. **Session file** (YAML): sources, filters, joins, emit, duration. Builds **the
   same `Request` object** as the CLI.
2. **`omn --dump-request`** prints the equivalent config: prototype on the CLI,
   freeze into a file. No divergence possible between the two surfaces.
3. **Resolved file emitted next to the results**: backend selected per source,
   placement of each filter, captured environment (kernel, BTF, microarchitecture,
   mitigations, clocksource, governor, SMT, THP mode, NUMA, virtualization, steal
   time, privileges). This is what makes two measurements comparable across
   machines.
4. **Variables and includes**, to replay a session across N machines or N
   workloads. A handful of variables is enough — do not reinvent Helm.

### Exit criteria

- [ ] A CLI session → file → replay produces an equivalent result.
- [ ] The resolved file is enough to explain a gap between two machines.

---

## Phase 8 — Self-benchmark

The initial PoC, turned against the tool itself. This is where `--backend force:X`
finally earns its place.

### Tasks

1. **Harness**: reference workloads (launch-bound, compute-bound, memory-bound,
   syscall-heavy, IO-heavy, contended multithread), warmup, ≥ 20 repetitions,
   round-robin between methods, median + IQR.
2. **Variance gate**: baseline without any tracer, measured CV. **If CV > 1 %, the
   bench cannot resolve a 1-3 % overhead** — fix the environment before measuring
   anything.
3. **Environment control**: `performance` governor, locked clocks, frozen THP,
   affinity, invalidation of any throttled run (`power.*` collected
   unconditionally).
   - **And the caches, which this list was missing.** A run leaves the last
     level cache and the NUMA node's memory in whatever state it left them, and
     the next run inherits it: `docs/scenarios/mem.md` already records that a
     shuffled walk over 64 MiB and one over 4 KiB differ by two orders of
     magnitude, which is the same effect seen on purpose. So the protocol
     evicts the LLC and sweeps each NUMA domain between repetitions, the way it
     already drops the page cache — a cold cache and a warm one are two
     measurements, and a harness that does not choose between them is measuring
     the order its runs happened to go in.
   - **Prefetcher state is captured, not controlled.** Disabling the hardware
     prefetchers is a real lever for isolating a memory effect, and it is also a
     modification of the target — boundary 5. So the state is read and written
     into the resolved file beside the governor and the THP mode, and whoever
     wants it off turns it off themselves and gets a resolved file that says so.
     Two runs on the same machine with different prefetcher settings are not
     comparable, and nothing today would say why.
4. **Backend comparison**: for each source, force every backend in the chain and
   measure cost, effective resolution, loss rate. **Validates that the resolver
   chooses correctly.**
5. **Effective resolution**: are two events X µs apart distinguished? Sweep X from
   0.5 to 50 µs per backend.
6. **Observer effect**: does the collector introduce serialization? Compare an
   asynchronous sequence with and without it.

### Exit criteria

- [ ] A cost/resolution/loss table measured for every (source, backend).
- [ ] The resolver's choices are confirmed by measurement, or corrected.
- [ ] The agent's own cost (CPU, RSS) is measured separately from the dilation of
      the observed process.

---

## Phase 9 — Broadening

Only now do we follow the catalog breadth-wise. Every added source is incremental
because the skeleton exists.

Suggested order: rest of `cpu` and `mem` → extended `io` and low-level `net` →
`proc` → advanced `mem` (bandwidth, c2c, wss) → `gpu` → `runtime` → `cpu.flow`.

`power` is not in that list: it has been collected unconditionally since phase 8
as a guardrail.

**Criterion per added source**: catalog entry + backend + fallback chain + ground
truth test + automatic appearance in `omn list`.

---

## Phase 10 — Intelligence layer

Above the engine, not inside it. It calls Omniscient like any other user.

The methods are the deliverable, not a menu. Each one names the sources it needs
and each is only worth building once they collect, so the order below is the
order in which their inputs land.

0. **The criterion comes first, and it is a parameter rather than a footnote.**
   "What is this workload limited by" has no answer until someone says *limited
   for what* — median latency, the tail, throughput, energy and cost do not name
   the same culprit, and improving one routinely degrades another. So a triage
   request carries its objective the way a measurement request carries its emit,
   and the answer names the objective it answered for. A layer that picks one
   silently is a layer that reports a confident wrong suspect, which is the
   failure this project is built against, one level up.
1. **USE triage** (Utilization / Saturation / Errors): a broad, cheap pass that
   answers "what is this workload limited by, for this objective?".
2. **TSA** (thread state analysis): where a thread's wall clock went, split
   across on-cpu, runnable-and-waiting, and blocked-on-what. **This one is
   ready first**, because its three inputs already collect and are checked --
   `cpu.profile`, `sched.runqlat` and `sched.offcpu` with its blocking stack --
   and the `wakeup` join already says what released a block. It is also the
   method that answers the question people actually arrive with, which USE does
   not: *why is this slow*, rather than *what is saturated*.
3. **Automatic escalation**: once the suspect is identified, enable that layer's
   detailed collectors and turn off the rest. The budget holds because we only pay
   dearly for one layer at a time. Depends on hot attach (phase 6).
4. **Question-oriented modes**: `omn latency --pid X`, `omn saturate`.
5. **Differential flame graphs**, which need nothing new: two profiles and a
   subtraction, and the encoders for both already exist.

### What this layer must not get wrong

Two arithmetics that look like presentation and are not:

- **Coordinated omission.** A `hist` built from a source that missed readings is
  biased, and biased *towards the answer the reader wants*: the readings a
  polled or sampled source loses are the ones taken while the machine was
  stalled, which are the slow ones. The collectors already count what they
  missed -- `late_ticks`, `throttled`, `dropped` -- and nothing anywhere says
  the distribution built on top of them therefore under-reports its own tail.
  This belongs to the emit layer, not to this phase, and it is recorded as
  finding 37 in `docs/open-findings.md`; it is named here because a triage layer
  reading a percentile is exactly who would be misled by one.
- **Little's law and the queueing identities** are how saturation is read from
  what is already collected -- `io.inflight` against `io.iops`, run queue length
  against `sched.runqlat`. They are worth stating as the arithmetic of the USE
  pass rather than leaving each source to imply them.

---

## Phase 11 — A model reads it

Last, and the ordering is the whole point. Omniscient already produces what a
language model needs and a human does not: typed events, attributions carrying
an evidence level, and a resolved file describing the machine, the backend and
every degradation. Handed that, a model can say what a run means without
inventing the parts it cannot see. Handed rendered text, it invents.

So the work is an MCP server exposing what already exists as resources — the
resolved file, the results document, the catalog — rather than a report
generator. Nothing new is measured and nothing new is modelled; this phase is a
second surface on the same objects, the way the session file is a second surface
on the same `Request`.

**It comes after phase 10 and not before.** A model summarising a correlator
nobody can yet interpret produces conclusions that are confident and wrong, and
the evidence ladder is exactly the thing it would flatten: `IDENTITY` and
`TEMPORAL` read alike in prose. Until the tool itself can say what a run was
limited by and for which objective, a model reading it has nothing to be right
about.

Two constraints that fall out of the rest of this file:

- **It reads; it does not run.** Boundary 5 applies to this surface too: a model
  may ask for a measurement and may not change the machine.
- **Every number it is handed carries what it is missing.** Finding 37's
  `Shortfall` exists for a reader taking a p99 off a lossy run, and a model is
  that reader with less judgement, not more.

---

## Cross-cutting test strategy

To be put in place from phase 3, not at the end. A tracer is hard to test because
it has no "obviously wrong" output.

| Level | Method |
|---|---|
| Ground truth | synthetic workloads with known behavior (`usleep`, `dm-delay`, `tc netem`, fixed-size IO) |
| Cost regression | the phase 8 harness, in CI |
| Portability | a VM kernel matrix: floor, LTS, recent, no BTF |
| Join robustness | injection of disorder, losses, recycled pointers |
| Degradation | run without privileges, without BTF, without tracefs — check that it degrades **while saying so** |

---

## Accepted boundaries, to be documented in the README from day one

1. **No bit-exact determinism.** PMU counters are stable (typical CV < 0.1 %) but
   never exact: skid, speculation, IRQs, SMT. Strict determinism belongs to
   dynamic binary translation, outside the kernel-level core.
2. **No inside view of NVIDIA GPUs.** CUPTI is closed, there is no kernel path.
   AMD and Intel are covered.
3. **Writes are less attributable than reads.** Writeback leaves in a kworker, the
   originating context is lost: `KEY` evidence, not `IDENTITY`.
4. **Single machine first.** Inter-machine clock drift is a different class of
   problem from local correlation.
5. **It observes; it does not tune.** Every knob a performance engineer reaches
   for is out of scope, and so is anything that modifies the target to measure
   it -- `ptrace` and `userfaultfd` are interception, not observation.
   `DECISIONS.md` carries the reasoning.
6. **LTTng and its kind are peers, not backends.** Same events, another path;
   nothing to recover by wrapping one. If LTTng-UST ever earns a place it is as
   a CTF **import**, a consumer of one more format.

These limits must be visible on first contact with the tool. A project named
"Omniscient" that fails to state them turns its boundaries into perceived bugs.
