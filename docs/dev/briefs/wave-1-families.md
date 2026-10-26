# Omniscient — parallel agent brief

One thread per source family, meant to run as independent agents on separate
branches. Read Part 0 and Part 1 before starting any thread: they are what keeps
nine agents from writing nine incompatible versions of the same layer.

Everything in this repository is written in English. The standing invariants in
`CLAUDE.md` apply to every thread without exception.

---

## Part 0 — Read this before dispatching anyone

### Nine agents cannot start today, and the reason is structural

A survey of the catalog's 155 backends:

| Family | Sources | bpf | pmu | fs | other |
|---|---|---|---|---|---|
| `cpu` | 14 | 1 | 13 | 7 | — |
| `mem` | 23 | 7 | 11 | 16 | 3 |
| `io` | 14 | 17 | — | 9 | — |
| `net` | 15 | 12 | — | 4 | 6 |
| `sched` | 10 | 12 | 1 | — | 1 |
| `power` | 5 | 3 | 2 | 5 | — |
| `gpu` | 9 | 1 | 2 | 4 | 11 |
| `proc` | 6 | 4 | — | 2 | — |
| `runtime` | 4 | 5 | — | 1 | 1 |

**62 of 155 backends are eBPF, and there is no eBPF layer.** `sched` has no
non-eBPF backend at all; `io`, `net`, `proc` and `runtime` are mostly eBPF.
Dispatching those agents today means five of them each inventing a loader, a
CO-RE strategy and a ring-buffer consumer, and then four of those being thrown
away.

So the threads below are grouped into waves, and one thread — `sched` — is
redefined: it builds the eBPF layer, because `sched.wakeup` is its first
customer and the roadmap's stated phase 3 target.

### Two Phase 0 decisions block the eBPF wave

`docs/DECISIONS.md` still has these open, and the eBPF layer cannot be designed
without them:

- **Kernel floor.** 5.15 means writing the futex and uprobe fallback for
  `sched.lock` and testing a path without BTF. 6.1 removes both and excludes the
  current Debian and Ubuntu LTS defaults.
- **Privileges.** The unprivileged path is not "fewer sources": it is a
  different first viable backend for most entries, which doubles what the
  resolver has to be tested against.

Settle both before Thread 5 starts. Nothing in Wave 1 depends on them.

### Groundwork that must land before any family thread

This is roughly one family's worth of work, and it is what turns six
conflicting agents into six independent ones. It has a single owner and it is
not parallel.

| # | What | Why it blocks |
|---|---|---|
| G1 | **Collector factory registry.** Replace the `match (source, backend)` in `src/collect/mod.rs` with a `Factory { source, backend, build }` list, and one `factories()` per family module. `implemented()` derives from it. | Every family adds a collector. A central match arm means every merge conflicts on the same three lines. |
| G2 | **Per-family collector modules.** `src/collect/<family>/mod.rs`, each exposing `pub fn factories() -> Vec<Factory>`. `src/collect/mod.rs` lists the nine once, and is then never edited again. | File-level ownership is what makes the merges trivial. |
| G3 | **Per-family test files.** `tests/family_<name>.rs`. Keep `tests/cli.rs` for the surface only. | One shared test file conflicts on every append. |
| G4 | **Probe classes for every requirement in the catalog**: `bpf`, `btf`, `kprobe`, `uprobe`, `usdt:*`, `fentry:*`, `symbols:*`, `netlink`, `sock_diag`, `rtnetlink`, `ethtool`, `af_packet:*`, `xdp`, `msr:*`, `hwmon`, `damon`, `rdt:*`, `so_timestamping`, `phc`, `vendor` libraries, `allocator-stats:*`, `dynamic-linking`. | Otherwise every agent edits `capabilities.rs`, and half of them decide an unprobeable requirement is "probably fine" — which the project forbids. |
| G5 | **The missing emits**: `hist` (log2 and linear), `topk`, `stack`. The catalog asks for `hist` 42 times, `topk` 52, `stack` 19. | Nearly every family needs them. Nine implementations is nine aggregation semantics. |
| G6 | **Stack capture and symbolization**, frame pointers first, `blazesym` or `libdw` later. Never write a DWARF unwinder. | `cpu.profile`, `sched.offcpu`, `mem.heap.alloc` by site all need it. |
| G7 | **`folded` and `speedscope` encoders**, once G6 exists. | Declared in `omn formats` and currently reported as not implemented. |
| G8 | **Perf layer extended**: counting mode (a `count` request opens no ring), frequency sampling (`freq` + `sample_freq`, not the period field), hardware events, callchain, per-cpu scope, grouped counters. | 30 pmu backends across `cpu`, `mem` and `power` need it. Today `src/perf.rs` only samples one software event on one task. |
| G9 | **Session-level `target:`**, pushed into requests that omit it. | A joined session repeats the same launch argv per request today. It is the one real ergonomic wart in the session format. |
| G10 | **eBPF layer** — Wave 2 only. libbpf-rs with CO-RE, a ring-buffer consumer reusing the reorder and loss accounting already in `collect`, and a privileged test path. Owned by Thread 5. | 62 backends. |

---

## Part 1 — Project structure and ownership

### Who owns what

A thread may create and edit **only** the paths listed as its own. Everything
else is read-only to it.

```
catalog/<family>.yaml              owned by that family's thread
src/collect/<family>/**            owned by that family's thread
tests/family_<family>.rs           owned by that family's thread
examples/<family>-*.yml            owned by that family's thread
docs/families/<family>.md          owned by that family's thread (findings, traps, measurements)
```

Shared, **not** editable by a family thread:

```
src/collect/mod.rs                 the nine-line family list, written once in G2
src/capabilities.rs                probe classes, written once in G4
src/perf.rs  src/bpf/**            mechanism layers, G8 and G10
src/emit.rs  src/encode.rs         aggregation and formats, G5 and G7
src/join.rs  src/request.rs        model, G9
src/resolve.rs src/validate.rs     resolver and validator
Cargo.toml  CLAUDE.md  README.md   coordinator only
docs/DECISIONS.md  docs/ROADMAP.md  docs/dev/BRIEF.md coordinator only
```

Need something from a shared file? **Do not edit it.** Write the request in
`docs/families/<family>.md` under a `## Needs from the shared layer` heading,
with the exact signature or probe class wanted and why the family cannot
proceed without it, and carry on with what is unblocked. The coordinator batches
these.

### Branches and isolation

One branch per thread, named `feat/<family>-collectors`, each in its own git
worktree. File sets are disjoint by construction, so merge order does not
matter and no rebase should ever conflict. If a merge does conflict, a thread
edited something it does not own; that is the bug, not the merge.

### Definition of done, per thread

A thread is finished when, for every source it claims:

1. The catalog entry is complete and truthful: ordered backends, real
   `requires` strings, a `degraded` note or `equivalent: true` on every
   fallback, unique backend ids within the chain, and caveats that state what
   the number does **not** mean.
2. A collector exists for at least the first viable backend, registered through
   the factory list.
3. `omn list <family>` shows the source as usable on a machine that can serve it
   and says why on one that cannot.
4. **A ground-truth test exists.** Not a test that the code returns what the
   code computed: a test against something independently known — a kernel
   counter in `/proc`, an injected delay, a known allocation size, a fixed
   number of iterations. Without one, a collector that returns a plausible
   constant passes everything else.
5. Losses are counted and surfaced. Zero everywhere is the only reading that
   means the numbers are complete.
6. `cargo test && cargo clippy --all-targets && cargo fmt --check` is clean.

---

## Part 2 — Invariants, non-negotiable

These were each learned the expensive way. Violating one is a correctness bug,
not a style preference.

1. **Unknown is never available.** A probe answers available, missing, or
   unknown. A backend whose requirements could not be checked is not selected
   while a verified one exists, and is reported as assumed when it is.
2. **Restricted is never absent.** A path that exists but cannot be read gets a
   different message from one that does not exist. One calls for privileges,
   the other for different hardware or a different kernel.
3. **Never degrade silently.** An unavailable source, a filter that stayed in
   userspace, a fallback backend, a dropped event: all of it surfaces.
4. **Never derive a number you cannot justify.** A "resident bytes" column
   computed as `faults × page size` was wrong by a factor of four on the
   development machine, because multi-size transparent huge pages make one fault
   install anywhere from 4 KiB to 62 KiB. If a derived value cannot be checked
   against something independent, report the raw one and say what it is not.
5. **Measure before explaining.** When a number is surprising, check it against
   the kernel's own counter before deciding why. Twice already the surprising
   number was right and the expectation was wrong.
6. **One clock domain.** `CLOCK_MONOTONIC` everywhere; `bpf_ktime_get_ns()` and
   never `bpf_ktime_get_boot_ns()`; device domains carry a non-zero `ts_err_ns`.
7. **Refuse rather than approximate.** A combination that cannot mean anything
   fails validation with the reason. Returning a system-wide number presented as
   per-process attribution is the worst bug this tool can have.
8. **Do not wrap `perf`, `bpftrace` or `bcc`.** Talk to `perf_event_open`,
   `bpf()`, procfs, sysfs and netlink directly.

---

## Part 3 — Waves

| Wave | Threads | Prerequisite |
|---|---|---|
| **0** | Groundwork G1-G9 | none; single owner; blocks everything |
| **1** | `cpu`, `mem`, `power`, `gpu` | G1-G8 |
| **1b** | `io`, `net`, `proc`, `runtime` — non-eBPF backends only | G1-G5 |
| **2** | `sched` (builds the eBPF layer first), then the eBPF backends of `io`, `net`, `proc`, `mem`, `runtime` | kernel floor and privileges settled |

Wave 1 and 1b run together: eight agents, disjoint files.

---

## Thread 1 — `cpu`

> **Mission.** Implement the `cpu` family of the Omniscient catalog: 14 sources,
> 13 of them `perf_event_open` counters or samplers, the rest procfs and cgroup
> files. You own `catalog/cpu.yaml`, `src/collect/cpu/**`,
> `tests/family_cpu.rs`, `examples/cpu-*.yml` and `docs/families/cpu.md`. Read
> `CLAUDE.md`, and Parts 1 and 2 of `docs/dev/BRIEF.md`, first.

**Sources.** `cycles`, `instructions`, `ipc`, `branch_misses`, `cache_misses`,
`topdown`, `profile`, `oncpu_time`, `context_switches`, `migrations`, `util`,
`pressure`, `throttle`, `flow`.

**Start with** `cpu.instructions` in `count` mode: it is the only source in the
whole catalog with `determinism: high`, so it is the one whose ground truth is
checkable to a fraction of a percent. A loop of N known iterations must count
within a few percent, and the run-to-run coefficient of variation must be under
1 %. If it is not, the environment is wrong before the code is.

**Traps, from the catalog and from measurement.**
- `cpu.instructions` is stable, never bit-exact: interrupt skid, speculation,
  IRQs, SMT. Do not write a test that demands an exact count.
- `cpu.topdown` is microarchitecture-specific. Map by CPU family; hard-coding
  event names for one vendor is a bug that will be found on someone else's
  laptop.
- `cpu.profile` needs the unwind mode declared. `dwarf` copies the stack on
  every sample: far more volume and cost than frame pointers. Depends on G6.
- `cpu.flow` (Intel PT / CoreSight) gives raw bytes; the decoder is a separate,
  late deliverable. Implement capture and say decoding is absent — do not
  half-decode.
- Disabling SMT or pinning affinity moves counters by 10-30 %. The environment
  capture already records SMT state; check it is in the resolved file.
- `cpu.ipc` is derived from two counters. It needs both in one group, or the
  ratio is of two things measured at different times.

**Needs from the shared layer.** G8, specifically counting mode and grouped
counters. Say so in `docs/families/cpu.md` if G8 lands short of that.

---

## Thread 2 — `mem`

> **Mission.** Finish the `mem` family: 23 sources, of which `mem.rss`,
> `mem.faults` and `mem.heap.alloc` already exist and are your reference for
> shape and rigour. You own `catalog/mem.yaml`, `src/collect/mem/**`,
> `tests/family_mem.rs`, `examples/mem-*.yml` and `docs/families/mem.md`. Read
> `CLAUDE.md`, and Parts 1 and 2 of `docs/dev/BRIEF.md`, first.

**Remaining sources.** `pss`, `uss`, `cgroup`, `page_alloc`*, `kmalloc`*,
`heap.live`, `heap.retained`, `cache_l1`, `cache_llc`, `cache_tlb`, `latency`,
`sharing`, `bandwidth`, `numa`, `wss`, `reclaim`*, `compaction`*, `thp`, `swap`,
`pressure`. Entries marked `*` are eBPF — Wave 2.

**Start with** `mem.heap.retained`. It is derived from `heap.live` and `rss`,
both of which are nearly free, and it is the metric that predicts OOMs in
production that almost nobody collects. It is also the family's best story.

**Traps.**
- The accounting metric is always explicit. RSS, PSS, USS, live heap, retained
  heap and `memory.current` are six different numbers, all correct. A request
  without a named metric fails; it does not pick one.
- `free()` returns nothing to the OS: the allocator keeps its arenas. That gap
  is exactly what `heap.retained` measures.
- `mem.bandwidth` is package-scoped. `--pid` must be refused, never degraded
  into a system-wide value presented as attribution. The validator already does
  this; do not weaken it.
- `mem.wss` via `page_idle` perturbs reclaim while it scans. DAMON does not.
  Declare the difference.
- Fault counts do not convert to bytes. See invariant 4; the measurement is in
  `docs/families/mem.md` once you create it, and in `docs/DECISIONS.md` now.

---

## Thread 3 — `power`

> **Mission.** Implement the `power` family: 5 sources, RAPL through
> `perf_event_open` plus sysfs and MSR readings. Small, self-contained, and
> load-bearing far beyond its size. You own `catalog/power.yaml`,
> `src/collect/power/**`, `tests/family_power.rs`, `examples/power-*.yml` and
> `docs/families/power.md`. Read `CLAUDE.md`, and Parts 1 and 2 of `docs/dev/BRIEF.md`,
> first.

**Sources.** `energy`, `freq`, `cstate`, `temp`, `throttle`.

**Why this family matters more than its size.** It is a confounder for every
other family. Thermal throttling and AVX-512 frequency licensing silently
corrupt CPU, memory and GPU measurements. The design intent is that the harness
collects `power.*` **at all times, whether or not the user asked**, and
**invalidates any throttled run**. Your deliverable is not only five collectors:
it is the mechanism by which a throttled run is marked unusable.

**Traps.**
- RAPL is per package. `--pid` is invalid. Per-process attribution is a derived
  estimate from cycle share and must be labelled as such, or not offered.
- RAPL counters wrap. A poll that misses a wrap reports a negative or absurd
  delta; handle it and count it.
- `scaling_cur_freq` is the requested frequency, not the effective one. The
  effective one comes from `aperf`/`mperf`. Declare which the backend gives.
- A throttled run is not a slower run: it is a different run. Averaging it in is
  the failure this family exists to prevent.

---

## Thread 4 — `gpu`

> **Mission.** Implement the `gpu` family: 9 sources across an asymmetric
> landscape — AMD and Intel expose a full kernel stack, NVIDIA exposes nothing
> below its closed SDK. You own `catalog/gpu.yaml`, `src/collect/gpu/**`,
> `tests/family_gpu.rs`, `examples/gpu-*.yml` and `docs/families/gpu.md`. Read
> `CLAUDE.md`, and Parts 1 and 2 of `docs/dev/BRIEF.md`, first.

**Sources.** `job`*, `engine_busy`, `freq`, `power`, `temp`, `mem`, `kernel`,
`api`, `counters`. `job` is eBPF on the DRM scheduler tracepoints — Wave 2.

**Start with** the sysfs sources — `freq`, `power`, `temp`, `mem` — which work
unprivileged on AMD and Intel today. Then `gpu.api` through the existing
`preload` mechanism, which needs no vendor SDK.

**Traps.**
- **Do not promise the inside of NVIDIA GPUs.** CUPTI is closed and there is no
  kernel path. The asymmetry is reflected in the catalog; keep it visible rather
  than papering over it with a vendor fallback that silently means something
  else.
- `gpu.job` measures a command buffer, not a kernel, IRQ included. It cannot say
  whether the GPU saturated its SMs or waited on VRAM.
- `gpu_scheduler` tracepoint fields change between kernel versions. Discover the
  format at runtime; prefer `(fence_context, seqno)` over a pointer.
- GPU timestamps are a different clock domain. Calibrate explicitly and set
  `ts_err_ns`; invariant 6 has no exception for convenience.
- `gpu.counters` does not measure time — kernel replay destroys the timeline.
  Its value is counter determinism, and it locks clocks by default.
- Most machines have no usable GPU. Every test must skip cleanly and say why,
  the way the perf tests already do.

---

## Thread 5 — `sched`, and the eBPF layer

> **Mission.** Build Omniscient's eBPF layer, and prove it with `sched.wakeup`
> and `sched.offcpu`. This is the phase 3 milestone of `docs/ROADMAP.md` and the
> prerequisite for 62 of the catalog's 155 backends. You own
> `catalog/sched.yaml`, `src/bpf/**`, `src/collect/sched/**`,
> `tests/family_sched.rs`, `examples/sched-*.yml` and `docs/families/sched.md`.
> Read `CLAUDE.md`, and Parts 1 and 2 of `docs/dev/BRIEF.md`, first. Do not start until
> the kernel floor and privileges lines of `docs/DECISIONS.md` are settled.

**Sources.** `wakeup`, `runqlat`, `offcpu`, `switches`, `migrations`, `lock`,
`futex`, `mutex`, `irq`, `numa_balance`.

**Order.** `sched.wakeup` first, and not for its own sake: it is the `WAKEUP`
join mechanism, the only way to attribute latency **across a block** without
instrumenting the application, and every other family's correlation depends on
it. Then `sched.offcpu`, which forces in-kernel aggregation.

**The layer.** libbpf-rs with CO-RE. Reuse what exists rather than rebuilding
it: `src/collect/mod.rs` already has the tick loop, the two-phase
prepare/attach, and loss accounting; `src/perf.rs` shows the ring-buffer
discipline expected, including publishing the tail only after a batch is copied
out.

**Traps.**
- `bpf_ktime_get_ns()` only. `bpf_ktime_get_boot_ns()` includes suspend time,
  and mixing the two is a silent bug that destroys every join with no error
  anywhere.
- `sched.offcpu` with stacks is expensive on switch-heavy workloads. Aggregate
  in the kernel — stack id to counter — rather than streaming raw events.
- `lock:contention_*` needs kernel ≥ 6.1. Below that, futex plus uprobes, with
  partial coverage stated.
- Ring buffers are per-CPU and arrive unsorted. A bounded window and a watermark
  are required: finalize nothing until every collector has passed T.
- Ground truth is not optional here and it is easy: a program that sleeps N ms
  must measure N ms ± 5 %, and `tc netem` or `dm-delay` inject known delays.

**Deliverable beyond the family.** Write `docs/bpf-layer.md`: how a program is
loaded, how maps are read, how losses are counted, and what the other families
must call. Four threads are waiting on it.

---

## Thread 6 — `io`

> **Mission.** Implement the `io` family: 14 sources spanning three layers that
> must never be conflated. Wave 1b covers the procfs and tracefs backends; the
> eBPF backends wait for Thread 5. You own `catalog/io.yaml`,
> `src/collect/io/**`, `tests/family_io.rs`, `examples/io-*.yml` and
> `docs/families/io.md`. Read `CLAUDE.md`, and Parts 1 and 2 of `docs/dev/BRIEF.md`,
> first.

**Available now:** `bytes`, `iops`, `inflight`, `merges`, `pressure` through
`/proc/diskstats`, `cgroup2 io.stat` and `/proc/pressure/io`. **Wave 2:**
`latency`, `queue`, `size`, `syscall`, `fs`, `pagecache`, `readahead`,
`writeback`, `fsync`.

**Traps.**
- Three layers, three different numbers: syscall latency is what the
  application suffers, block latency is what the device does, FS latency is
  journal and locks and allocation. **The gap between them is the diagnosis.**
  Never report one as the other.
- `io.latency --pid` is reliable **for reads only**. Reads are submitted in the
  reader's context so `bpf_task_storage` is readable at `block_rq_issue`
  (`IDENTITY`). Writes leave through writeback in a kworker: context gone, fall
  back to `file:dev+ino` (`KEY`). The evidence level differs per direction and
  must be reported per direction.
- Request pointers are recycled. The key is `(pointer, open timestamp)`, scoped
  equality, never bare pointer equality. The join engine's
  `a_reused_address_does_not_inherit_the_previous_allocations_faults` test shows
  the failure this prevents.
- A cold page cache and a warm one differ by an order of magnitude. A benchmark
  that mixes them produces a bimodal distribution that then gets called noise.
  Declare which the protocol wants and enforce it.
- SSDs drift: internal GC, write cache, SLC buffer. Precondition, and watch
  inter-run drift, or test ordering decides the result.

---

## Thread 7 — `net`

> **Mission.** Implement the `net` family: 15 sources. Wave 1b covers netlink,
> procfs and `AF_PACKET`; the eBPF backends wait for Thread 5. You own
> `catalog/net.yaml`, `src/collect/net/**`, `tests/family_net.rs`,
> `examples/net-*.yml` and `docs/families/net.md`. Read `CLAUDE.md`, and Parts 1
> and 2 of `docs/dev/BRIEF.md`, first.

**Available now:** `tcp.rtt` via `sock_diag`, `bytes` and `packets` via
`/proc/net/dev` and rtnetlink, `backlog` via netstat, `iface` via ethtool,
`capture` via TPACKETv3, `hwts` via `SO_TIMESTAMPING`. **Wave 2:**
`tcp.connect`, `tcp.lifetime`, `tcp.retransmit`, `drops`, `socket`, `qdisc`,
`softirq`, `xdp`.

**Traps.**
- The key that crosses the whole stack is `sock:cookie`, not the PID. On
  receive, processing happens in softirq, outside any process context. A
  per-PID attribution of received bytes is wrong in a way that looks right.
- `net.hwts` is the only way to settle "is it the network or is it us": it
  splits latency into NIC, kernel and application segments. It is also a
  different clock domain — PHC — so calibration and `ts_err_ns` are mandatory.
- `net.capture` silently produces wrong conclusions when it drops under load.
  Buffer drops must be counted and surfaced.
- The `skb:kfree_skb` drop reason field only exists on recent kernels. Discover
  the tracepoint format at runtime.
- Multi-host clock drift is out of scope. Do not start down that road.

---

## Thread 8 — `proc`

> **Mission.** Implement the `proc` family: 6 sources on processes and syscalls.
> Wave 1b covers `fd` and `threads` through procfs; the rest wait for Thread 5.
> You own `catalog/proc.yaml`, `src/collect/proc/**`, `tests/family_proc.rs`,
> `examples/proc-*.yml` and `docs/families/proc.md`. Read `CLAUDE.md`, and Parts
> 1 and 2 of `docs/dev/BRIEF.md`, first.

**Available now:** `fd`, `threads`. **Wave 2:** `syscall`, `exec`, `exit`,
`signals`.

**Traps.**
- `raw_syscalls` fires on *everything*. A raw trace on a syscall-heavy workload
  is not a measurement, it is a load test. Aggregate in the kernel or filter by
  syscall number inside the program.
- `--cgroup` is not a PID filter. Without the pid namespace, events are
  attributed to the wrong process — hence the `proc:pidns+pid` key. A `tid`
  alone is ambiguous the moment containers exist.
- `proc.fd` against the limit is the useful form; the count alone says nothing.

---

## Thread 9 — `runtime`

> **Mission.** Implement the `runtime` family: 4 sources covering the layer
> above the kernel — GC, JIT, allocator, interpreter — plus the extension point
> where users declare their own probes. You own `catalog/runtime.yaml`,
> `src/collect/runtime/**`, `tests/family_runtime.rs`, `examples/runtime-*.yml`
> and `docs/families/runtime.md`. Read `CLAUDE.md`, and Parts 1 and 2 of
> `docs/dev/BRIEF.md`, first.

**Sources.** `gc`, `jit`, `alloc`, `custom`.

**Available now:** `runtime.alloc` through the existing `preload` mechanism —
`src/collect/heap.rs` and `shim/` are your reference, and the shim may need
extending rather than duplicating. `runtime.jit` can already read
`/tmp/perf-<pid>.map` for symbol names. **Wave 2:** the USDT backends.

**Why it matters.** Without this layer, a GC pause or a JIT deoptimization is
attributed to the wrong resource: it appears as CPU time, or as memory growth,
with no way to tell.

**Traps.**
- USDT probes exist only if the runtime was built with them: Java needs
  `+ExtendedDTraceProbes`, Python needs `--with-dtrace`. Absence is the normal
  case and must be reported as absence, not as "no GC pauses occurred".
- `runtime.custom` is user-declared, so its validation cannot come from the
  catalog. Decide how a user declares a probe and how the tool refuses one it
  cannot attach — and refuse loudly.
- A uprobe costs 1-2 µs per hit. On a hot call site that is not an observation,
  it is a different program. The `preload` path exists for exactly this reason.

---

## Part 4 — Coordinator checklist

- [ ] Kernel floor and privileges settled in `docs/DECISIONS.md` (blocks Wave 2)
- [ ] G1-G9 landed on `main` (blocks Wave 1)
- [ ] `docs/families/` created with one stub per family
- [ ] Nine worktrees, nine branches `feat/<family>-collectors`
- [ ] Wave 1 and 1b dispatched: `cpu`, `mem`, `power`, `gpu`, `io`, `net`,
      `proc`, `runtime`
- [ ] Thread 5 dispatched once its two decisions are settled
- [ ] G10 and `docs/bpf-layer.md` reviewed before the Wave 2 backends start
- [ ] Each merge: file set disjoint, ground-truth test present, losses surfaced
