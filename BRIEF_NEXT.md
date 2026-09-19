# Brief, next — the twenty-four the mechanism already reaches

**Not for dispatch yet.** The fix wave is running against `BRIEF.md` and touches
`src/collect/**`; every family stream below would conflict with its streams E
and F. This is the wave after that one, written now so the shape is settled
before it is needed.

---

## The finding this brief exists for

`src/bpf/attach.rs` has classic tracepoints, kprobes and uprobes, built by wave
three and **used by nothing**. Every eBPF program in the tool still attaches
through `BPF_RAW_TRACEPOINT_OPEN`, which was the only path when they were
written.

So the largest block of remaining breadth is not blocked on mechanism. It is
twenty-four collectors waiting to be written against an attach layer that
already exists and is already probed:

```
bpf:tp      20   mem.page_alloc mem.kmalloc mem.reclaim mem.compaction
                 io.latency io.queue io.size io.pagecache io.readahead io.writeback
                 net.tcp.lifetime net.tcp.retransmit net.drops net.qdisc net.softirq
                 proc.syscall proc.exec proc.exit proc.signals
                 gpu.job
bpf:raw_tp   4   sched.lock sched.futex sched.irq sched.numa_balance
```

Every tracepoint they name exists on this kernel except `gpu_scheduler:*`,
which the proprietary NVIDIA driver does not publish — so `gpu.job` is a
collector that will be written here and exercised elsewhere.

This is family work, so it divides by family, the way the first wave did and
for the same reason: one directory each, merges that cannot conflict.

---

## Phase one — the collectors

| | Stream | Sources | Owns |
|---|---|---|---|
| **A** | `io` through the block layer | `io.latency`, `.queue`, `.size`, `.pagecache`, `.readahead`, `.writeback` | `src/bpf/io*.rs`, `src/collect/io/**` |
| **B** | `net` through skb and sock | `net.tcp.lifetime`, `.tcp.retransmit`, `.drops`, `.qdisc`, `.softirq` | `src/bpf/net*.rs`, `src/collect/net/**` |
| **C** | `mem` through the allocator | `mem.page_alloc`, `.kmalloc`, `.reclaim`, `.compaction` | `src/bpf/mem*.rs`, `src/collect/mem/**` |
| **D** | `proc` through process events | `proc.syscall`, `.exec`, `.exit`, `.signals` | `src/bpf/proc*.rs`, `src/collect/proc/**` |
| **E** | `sched` contention and interrupts | `sched.lock`, `.futex`, `.irq`, `.numa_balance` | `src/bpf/sched*.rs`, `src/collect/sched/**`, `src/collect/gpu/**` and `gpu.job` |

Five streams, each owning one family's directory and its own files under
`src/bpf/`. **Nobody owns `src/bpf/attach.rs`, `insn.rs`, `map.rs`, `btf.rs` or
`syscall.rs`** — those are the shared layer, and a stream needing something from
them writes the request in `docs/open-findings.md` rather than editing them. If
two streams need the same thing, the coordinator lands it once.

### Stream A is the one that matters most

`io.latency` and `io.queue` both join on `blk:rq@open_ts` with `scoped_eq` — the
one match kind the engine implements and **no source produces**. Landing them
turns a third of the join engine from theory into something checked, and it is
what phase two's I/O path is waiting for.

They are also the reason the `blk` key is scoped. A request pointer is recycled
within milliseconds, so `(ptr, open_ts)` is the identity and `ptr` alone is a
key that silently attributes one request's latency to another. That is written
into the catalog already; the collector has to honour it.

### What every stream does

- **Aggregate in the kernel where the source is a distribution.** `io.latency`
  is a histogram or it is nothing, and `src/bpf/runqlat.rs` is the worked
  example: `(key, bucket) -> count` with an unrolled log2 search, and
  `emit::WEIGHT_ATTR` on each row so a bucket standing for four hundred
  measurements counts as four hundred.
- **Attach through `attach::tracepoint`**, not `raw_tracepoint_open`. Raw
  tracepoints need BTF and a name; classic ones need a numeric id out of tracefs
  and `PERF_EVENT_IOC_SET_BPF`. Tracefs is root-only on a stock machine, which
  is why the raw path is ordered first where both work — and these twenty name
  events the raw path cannot reach.
- **Check the tracepoint's prototype before building instructions.**
  `offcpu.rs` refuses a kernel whose `sched_switch` has a different signature,
  because a program written for one shape reads a `task_struct *` out of a slot
  holding an integer and gets a plausible number. Every one of these needs the
  same guard.
- **Write the scenarios with the collector.** Four classes, as `BRIEF.md` of the
  scenario wave defined them and `GUIDE.md` explains them: ground truth, the
  capability sweep, the loss path, the refusal. A collector without them is half
  the work, and the fourth wave is what established that.

---

## Phase two — the crossings

Four streams, each a question a single family cannot answer. **P3 needs phase
one's stream A**; the others do not.

| | Stream | The question | The join |
|---|---|---|---|
| **P1** | Memory, end to end | Which allocation cost memory, and did it stay? | `heap.alloc → mem.faults` (interval, works today), then `→ mem.rss` and `heap.retained` |
| **P2** | Compute, end to end | Where did the time go — on cpu, or waiting? | `cpu.profile` beside `sched.offcpu` by task, and `sched.wakeup → sched.offcpu` (wakeup) for what released it |
| **P3** | The I/O path | What did one read cost, from syscall to platter? | `blk:rq@open_ts` (scoped_eq), which phase one's stream A produces |
| **P4** | Crossed families | Does the machine's own state explain the measurement? | `power.freq` and `power.throttle` against `cpu.instructions`; `mem.pressure` against `io.bytes` |

The scenario wave left these possible on purpose: every session carries its
keys, and each family's document has a `## Workloads` section naming what its
workloads guarantee. P2 needs a program half on-cpu and half blocked —
`examples/sched-half-blocked.yml` is it.

Each stream owns `examples/cross-<name>-*.yml`, `tests/cross_<name>.rs` and
`docs/scenarios/cross-<name>.md`. The prefix is the lock, which is what gave the
scenario wave zero conflicts across eight streams.

---

## What is left after this, and why each is its own decision

Not in this brief. Written down so it is not rediscovered.

| Sources | Needs | Why it is a decision and not a task |
|---|---|---|
| `io.syscall`, `io.fs`, `io.fsync`, `net.tcp.connect`, `net.socket` | **fentry** — `BPF_PROG_TYPE_TRACING` with an `attach_btf_id` | Real mechanism work, and the last attach kind worth having. Cheaper per call than a kprobe and typed by BTF, so it fails to load rather than reading the wrong field. |
| `runtime.gc`, `runtime.alloc`, `runtime.custom` | **USDT** — the target's ELF notes, then a uprobe | The uprobe half exists. What is missing is reading `.note.stapsdt` and the semaphore protocol. `docs/open-findings.md` records that a preload shim costs 132 ns against a uprobe's 740 ns for the same event. |
| `mem.latency`, `mem.sharing` | **PEBS or IBS** | A different sampling shape — the hardware records the address, not just the instruction pointer. |
| `mem.bandwidth` | **uncore PMUs** | A different sysfs layout and a different privilege story. |
| `cpu.flow` | **Intel PT** | An AUX buffer and a decoder. Large, and this machine is AMD. |
| `sched.mutex`, `gpu.api` | **preload** | The shim exists for malloc. Interposing `pthread_mutex_lock` and the GL/Vulkan entry points is the same mechanism pointed elsewhere. |
| `gpu.kernel`, `gpu.counters` | **CUPTI, Nsight** | Vendor libraries with their own licensing and their own idea of what a profile is. |
| `net.xdp` | **an XDP program** | A different program type and a different attach point, and the only one here that can drop packets rather than observe them. |

Two more that are decisions rather than mechanism:

- **`net.capture` needs `CAP_NET_RAW`**, which the build does not grant. The
  capability lets the binary read every frame on the machine. The scenario wave
  deliberately left this to be decided rather than deciding it.
- **`Unit::Watts` does not exist**, which is part of why `gpu.power` reports an
  energy counter, and the GPU is not in the environment capture — so two runs
  are comparable only if they were taken on the same device under the same
  driver.

---

## Rules

The same three that have held for five waves:

1. **The unit of ownership is a file set nobody else writes.** Here it is one
   family directory plus that family's own files under `src/bpf/`. The shared
   eBPF layer belongs to nobody.
2. **The handoff is a file only that stream can write.** `docs/fixes/` and
   `docs/scenarios/` worked; a "write a log at the end" instruction did not.
   Yours is `docs/families/<family>.md`, extended rather than replaced.
3. **A source is landed when its scenarios are.** Ground truth, the capability
   sweep, the loss path, the refusal. `GUIDE.md` explains each and
   `tests/scenario_<family>.rs` is where they go.

Read `STATE.md`, `docs/bpf-layer.md` for the calling conventions, and
`src/bpf/runqlat.rs` for in-kernel aggregation with a histogram. Build with
`make`: these all need `CAP_BPF` and `CAP_PERFMON`, and a green suite on an
unprivileged binary says nothing about any of them.
