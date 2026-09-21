# Brief — the shared layer

Six work streams, meant to run as independent agents on separate branches. This
is the second fan-out on this project and it is divided differently from the
first, for a reason worth understanding before dispatching anyone.

The first wave split by **family**, because the work was one collector per
source and the file sets were naturally disjoint: nine agents, nine directories,
merges that could not conflict. That work landed — 42 of 100 sources collect.

What is left is mostly **shared layer**, and shared layer does not split by
family. It splits by subsystem, the file sets are smaller and the seams between
them are real, so this brief is explicit about who owns which file and what each
stream must ask for rather than take.

Read `STATE.md` for where the project is, `docs/contributing.md` for what ground
truth means, and `CLAUDE.md` for the invariants. They apply to every stream.

---

## The streams

| | Stream | Owns | Blocks |
|---|---|---|---|
| **A** | Aggregation and presentation | `src/emit.rs`, `src/encode.rs`, `src/format.rs` | six families, and the profile formats |
| **B** | Stack capture | `src/stack.rs` (new), the callchain of `src/perf.rs` | `cpu.profile`, `sched.offcpu`, every `--emit stack` |
| **C** | The join engine | `src/join.rs` | the whole of roadmap phase 4 |
| **D** | The catalog model | `src/catalog.rs`, `src/resolve.rs`, `src/validate.rs`, `src/event.rs`, `src/request.rs`, and the `targets:`/`filter_fields:`/`keys:` fields of `catalog/*.yaml` | correct backend choice, per-device series |
| **E** | eBPF attachment | `src/bpf/**`, `src/collect/sched/**` | `io`, `net`, `runtime`, `sched.offcpu` |
| **F** | The honest small ones | `src/collect/mod.rs`, `src/main.rs`, `src/capabilities.rs`, `src/cli.rs` | nothing, and they are all things the tool currently gets wrong |

**A and B share a seam** and must agree on it before either starts: a captured
stack is carried on an event as an attribute plus a side table in `Results`, not
as a new field on `Event`. That keeps `Event` — which D owns — out of it. B
produces and symbolizes; A aggregates and renders.

**D touches every catalog file** but only three fields of each. A family thread
started in parallel would conflict; none is dispatched in this wave for that
reason.

**E owns `src/collect/sched/**`** because `sched.offcpu` is the first customer of
in-kernel aggregation and there is no sense in writing the map support without
the source that proves it.

Everything else under `src/collect/<family>/` is untouched by this wave. Family
breadth is wave two, and most of it is blocked on A, B or E.

---

## Ordering

Nothing here is strictly sequential, but two streams unblock others and should
start first: **A**, because six families have sources they cannot express
without `hist` and `topk`, and **D**, because the `Dev` and `Cpu` keys it adds
are what A needs for a per-device `topk`.

If only two agents can run, run A and C: `hist` is the most-asked-for thing in
the catalog, and the `wakeup` match is the highest-value single item left.

---

## Rules, the same as last time

A stream may create and edit **only** the paths listed as its own. Everything
else is read-only to it. Needing something from another stream's files is not a
reason to edit them: write the request under a `## Needs from another stream`
heading in `docs/open-findings.md`, with the exact signature wanted and why, and
carry on with what is unblocked.

One branch per stream, `feat/<stream>`, in its own worktree. If a merge
conflicts, a stream edited something it does not own; that is the bug, not the
merge.

A stream is done when `make check` is clean, the behaviour it adds has a test
that would fail without it, and anything it discovered that belongs to someone
else is written down rather than fixed in passing.

---

## Stream A — aggregation and presentation

> **Mission.** Implement the three emits the catalog asks for 113 times and that
> do not exist: `hist`, `topk` and the aggregation half of `stack`. You own
> `src/emit.rs`, `src/encode.rs` and `src/format.rs`. Read `CLAUDE.md`,
> `docs/contributing.md` and `STATE.md` first.

**What exists.** `raw`, `timeline`, `summary` and `rate`. `Results` carries an
`Output` per emit; `encode` renders each in text, json, yaml, toml and ndjson.

**What to build.**

- **`hist`**, log2 and linear. The catalog asks for it 42 times, and for
  `io.latency` a histogram is not a presentation choice: a latency distribution
  is bimodal when a page cache is cold and warm in the same run, and a mean
  hides exactly that. Log2 buckets are also what in-kernel aggregation produces
  cheaply, which is what makes the overhead viable later — so the bucket layout
  should be the one an eBPF map would build, not a prettier one.
- **`topk`**, 52 asks. The interesting part is what a series *is*: `topk` over
  what? Devices for `io.bytes`, allocation sites for `mem.heap.alloc`, sockets
  for `net.tcp.rtt`. Today a series is keyed on an event's first key, which is a
  convention with no name. Give it one.
- **`stack`**, 19 asks, once stream B can capture one. Aggregate by stack
  identity, not by frame list, or a profile of a deep stack costs more to
  aggregate than to collect.
- **The `folded` and `speedscope` encoders**, which are declared and refuse
  today. `folded` is `main;work;read 1234` per line and is trivial; speedscope
  carries both sampled profiles and evented timelines, so it is also the format
  a timeline should offer.

**Traps.**

- A run's window is not the requested duration. `rate` already divides by the
  observed window and reports no rate at all when there is none; a histogram
  over a window that did not happen is the same class of lie.
- Two emits on one request cost one collection. Keep it that way.
- `format.rs` declares what each format can carry and whether its encoder
  exists. A test crosses the claim with reality; keep it true.

**Ground truth.** A histogram of a known distribution — a program that sleeps
1 ms a hundred times and 100 ms twice — must show two modes, and the counts must
be exact. `topk` of a known ranking must order it correctly.

---

## Stream B — stack capture

> **Mission.** Capture and symbolize call stacks, so that a profile can say
> *where*. You own `src/stack.rs` (new) and the callchain parts of
> `src/perf.rs`. Read `CLAUDE.md`, `docs/contributing.md` and `STATE.md` first.

**What exists.** `SampledEvent` requests `IP | TID | TIME | ADDR` and no
callchain. `--stack fp|dwarf|lbr|none` is parsed, validated and unused.

**What to build.**

- `PERF_SAMPLE_CALLCHAIN` on a sampled event, and the record parsing for it.
- Frame pointers first. **Do not write a DWARF unwinder** — `blazesym` or
  `libdw` when the time comes, and `dwarf` should report that it is not
  implemented rather than silently unwinding one frame.
- Symbolization: address to function name, through the target's own maps.
- A stack identity, so that the same stack seen a thousand times is stored once
  and aggregated by id. This is the seam with stream A: agree the shape before
  writing it, and carry it as an attribute plus a side table rather than a new
  field on `Event`, which stream D owns.

**Traps.**

- `dwarf` copies the stack on every sample: far more volume and cost than frame
  pointers, and the request carries the mode for exactly that reason. Say what
  it costs where the user chooses it.
- A program built without frame pointers yields one frame. That is a
  degradation, not a result: report it the way a fallback backend reports one.
- A kernel stack and a user stack are different things and `perf` returns both
  with a marker between them. Keeping them apart is the difference between "this
  program is slow" and "this program is waiting on the kernel".

**Ground truth.** A program with a known call chain — `a` calls `b` calls `c`,
spinning in `c` — must produce that chain and no other, with the frames in that
order.

---

## Stream C — the join engine

> **Mission.** Implement the three match kinds the engine refuses by name, and
> make the wakeup chain a correlation rather than a stream of events. You own
> `src/join.rs`. Read `CLAUDE.md`, `docs/contributing.md` and `STATE.md` first.

**What exists.** Declarative rules, `interval` matching, open-state lifecycle
with a window, a bounded capacity and eviction counters that reach the output.
`eq`, `scoped_eq` and `wakeup` are refused by name.

**What to build.**

- **`eq`** — the simple one, and the one that makes a second real join possible
  today, since `sched.wakeup` and `sched.switches` both carry `task`.
- **`scoped_eq`** — `(pointer, open timestamp)`. Kernel pointers are recycled,
  and a bare equality produces false matches. The catalog already writes
  `blk:rq@open_ts` for this.
- **`wakeup`** — the point of the exercise. It is the only join that crosses a
  block, and every cross-layer story in `ROADMAP.md` phase 4 depends on it: a
  request that waited 41 ms on a lock is attributable only through the chain of
  who woke whom.

**Traps, from the thread that built `sched.wakeup`.**

- **A waker can be the task that was merely interrupted.** Softirqs run in the
  interrupted task's context, so a process doing nothing but `sleep 0.3` appears
  to wake `rcu_preempt`, a kworker and a network daemon. A waker of pid 0 is the
  idle task. The join is only as good as the waker, and the evidence level has
  to say so.
- A wakeup chain is a graph, not a pair: A wakes B wakes C. Decide what depth
  means and what a cycle means before the data decides for you.
- An open state that is never closed is a leak, and a leak in a join engine is a
  process that grows until it is killed halfway through a measurement. The
  window, the capacity and the eviction counters already exist; use them.

**Ground truth.** Two processes and a pipe: the reader blocks, the writer
writes, and the reader must be attributed to the writer with `WAKEUP` evidence.
A third, unrelated process must not appear in that chain.

---

## Stream D — the catalog model

> **Mission.** Close the modelling gap three threads hit independently, and add
> the two keys whose absence makes a per-device series impossible. You own
> `src/catalog.rs`, `src/resolve.rs`, `src/validate.rs`, `src/event.rs`,
> `src/request.rs`, and the `targets:`, `filter_fields:` and `keys:` fields of
> every `catalog/*.yaml`. Read `CLAUDE.md`, `docs/contributing.md`,
> `STATE.md` and finding 1 of `docs/open-findings.md` first.

**The gap.** A catalog entry declares one `targets` map, one `filter_fields`
list and one `keys` list for a whole backend chain, and all three actually vary
per backend.

- `io.bytes --system` resolves onto `fs:cgroup`, a backend whose whole purpose
  is per-cgroup attribution, and fails with "no collector". Reordering the chain
  would fix that request and break `--cgroup`, which would then land on
  `/proc/diskstats` and be answered with the whole machine's I/O — the worst bug
  this tool can have.
- 26 sources declare a filter field no collector emits. Since the userspace
  filter runs, a predicate over one is false everywhere and empties the series.
  `comm` on `net.bytes` is real for the eBPF backend and impossible for
  `/proc/net/dev`, which has no idea which process moved what.

**What to build.**

- Optional `targets` and `filter_fields` on `Backend`, falling back to the
  entry's when absent, and a resolver that skips a backend that cannot serve the
  requested target the way it already skips one whose requirements are missing —
  saying so in the chain it prints.
- `KeyValue::Dev { major, minor }` and `KeyValue::Cpu { cpu }`, in the shape
  `Iface` already has. Without them the four `io` diskstats sources sum every
  device into one number, `power` cannot produce a per-cpu series, and the
  `topk` over devices those entries declare cannot be expressed at all.
- Then fix the 26: per source, emit the attribute or take the field out. The
  table is in `docs/open-findings.md`. Where a field is real for one backend and
  impossible for another, that is what the new per-backend field is for.

**Traps.** This stream changes what the resolver chooses, which is the one thing
every other stream's tests depend on. Expect to find tests asserting a backend
by name that were really asserting "the right one"; fix the assertion to the
invariant, not to the new name.

**Ground truth.** `io.bytes --system` and `io.bytes --cgroup <path>` must resolve
onto different backends, each the one that can answer, and neither must answer
with the other's number.

---

## Stream E — eBPF attachment

> **Mission.** Take the eBPF layer past raw tracepoints, and prove it with
> `sched.offcpu`. You own `src/bpf/**` and `src/collect/sched/**`. Read
> `CLAUDE.md`, `docs/contributing.md`, `STATE.md` and `docs/bpf-layer.md`
> first. You need `CAP_BPF` and `CAP_PERFMON`: `make` grants them, and a rebuild
> drops them.

**What exists.** A loader with no clang and no build step: a program is an
instruction array, and `task_struct` offsets come from `/sys/kernel/btf/vmlinux`
at load time. Raw tracepoints attach; `sched.wakeup` loads, verifies, attaches
and produces its causal chain for real.

**What to build, in this order.**

- **In-kernel aggregation** — hash maps, `bpf_map_update_elem`, walking a map
  from userspace. `sched.offcpu` needs it and cannot exist without it: a raw
  trace of every context switch on a switch-heavy workload is a load test, not a
  measurement. Two million events becoming a few hundred bytes is what makes the
  overhead viable, and it is the mechanism `--budget` will degrade onto.
- **`sched.offcpu`**, the first customer: blocked time with the stack that
  blocked. Coordinate with stream B for the stack; `bpf_get_stackid` is the
  kernel-side half.
- **kprobes and fentry**, which `io` and `net` need for `vfs_*`, `tcp_*` and the
  block layer.
- **uprobes and USDT**, which `runtime` needs. Note the measurement already
  made: a preload shim costs 132 ns per call against 740 ns for a uprobe on the
  same event, so a uprobe is the fallback and not the first choice.

**Traps.** `bpf_ktime_get_ns()` only — `bpf_ktime_get_boot_ns()` includes
suspend time, and mixing the two is a silent bug that destroys every join with
no error anywhere. Ring buffers are per-CPU and arrive unsorted. The kernel floor
is 6.1 and below it the affected sources are refused by name, with the version.

**Ground truth.** A program that sleeps N ms must show N ms off-cpu, ± 5 %. A
program blocked on a lock must show the lock, not the sleep.

---

## Stream F — the honest small ones

> **Mission.** Six things the tool currently gets wrong or leaves unsaid, none
> large, all of them the kind of thing this project exists not to do. You own
> `src/collect/mod.rs`, `src/main.rs`, `src/capabilities.rs` and `src/cli.rs`.
> Read `CLAUDE.md`, `docs/contributing.md`, `STATE.md` and section 4 of
> `docs/open-findings.md` first.

1. **A throttled perf event is a loss of unknown size.** `sched.switches` counts
   it into `dropped`; `mem.faults` ignores it entirely, so a throttled run looks
   complete. Expose `SampledEvent::throttled()`, give `Losses` a `throttled`
   count of its own — folding it into `dropped` overstates precision, because
   the number of events lost inside a throttled period is not knowable — and
   make every sampling collector report it.
2. **The closing read of a polled counter.** `net` takes one on the grounds that
   without it the last interval of every run is dropped; `io` and `power`
   deliberately do not, because `run_all` samples and *then* decides to stop, so
   a closing read produced a second point at the same offset with a value of
   zero that dragged every summary. Both cannot be right. Either the runner
   stops before its last sample or no counter collector reads at the edge —
   decide, apply it everywhere, and write the decision in `DECISIONS.md`.
3. **An exit code for "valid, but this mechanism is not built".** A mode a
   collector cannot serve returns `CollectError::Start` and exits 1, like a real
   failure. `NoCollector` exits 3 and this belongs in the same class.
4. **`--budget` is parsed and ignored.** Record it in the resolved request and
   report what it would have degraded, even before anything degrades: a budget
   that silently does nothing is worse than one that is refused.
5. **`Unit::Watts` does not exist**, which is part of why `gpu.power` reports an
   energy counter rather than an instantaneous draw.
6. **The GPU is not in the environment capture.** Two runs are comparable only
   if they were taken on the same device under the same driver. It is not free —
   `nvmlInit` costs 3.3 to 4.9 ms and `Capabilities::detect` runs on every
   command including those that measure nothing — so it needs a capture that
   knows what the request is about.

**Ground truth.** For each: a test that fails without the change. The throttle
one is reachable by sampling a hot loop at a frequency above
`kernel.perf_event_max_sample_rate`.
