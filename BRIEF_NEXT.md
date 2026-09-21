# Brief, next — the crossings, and the layer above them

**Not for dispatch yet.** The sixth wave is running against `BRIEF.md`: five
family streams writing eBPF collectors and one rewriting the emit layer. Every
stream below reads what those produce, and P3 cannot start at all until stream A
has landed `io.latency`.

Written now so the shape is settled before it is needed, which is the habit that
has held for three waves.

---

## Phase two — the crossings

Four streams, each a question a single family cannot answer. **P3 needs the
sixth wave's stream A**; the others do not.

| | Stream | The question | The join |
|---|---|---|---|
| **P1** | Memory, end to end | Which allocation cost memory, and did it stay? | `heap.alloc → mem.faults` (interval, works today), then `→ mem.rss` and `heap.retained` |
| **P2** | Compute, end to end | Where did the time go — on cpu, or waiting? | `cpu.profile` beside `sched.offcpu` by task, and `sched.wakeup → sched.offcpu` (wakeup) for what released it |
| **P3** | The I/O path | What did one read cost, from syscall to platter? | `blk:rq@open_ts` (scoped_eq), which the sixth wave's stream A produces |
| **P4** | Crossed families | Does the machine's own state explain the measurement? | `power.freq` and `power.throttle` against `cpu.instructions`; `mem.pressure` against `io.bytes` |

The scenario wave left these possible on purpose: every session carries its
keys, and each family's document has a `## Workloads` section naming what its
workloads guarantee. P2 needs a program half on-cpu and half blocked —
`examples/sched-half-blocked.yml` is it.

Each stream owns `examples/cross-<name>-*.yml`, `tests/cross_<name>.rs` and
`docs/scenarios/cross-<name>.md`. The prefix is the lock, which is what gave the
scenario wave zero conflicts across eight streams.

### P2 is closer than it looks, and is where phase 10 starts

`ROADMAP.md` phase 10 now names **TSA** — thread state analysis, where a
thread's wall clock went across on-cpu, runnable-and-waiting, and blocked-on-what
— and puts it ahead of USE, because its three inputs already collect and are
checked: `cpu.profile`, `sched.runqlat` and `sched.offcpu` with its blocking
stack, plus the `wakeup` join for what released the block. P2 is that
decomposition for one task. It is the last stream here that needs nothing new,
and the first piece of the intelligence layer.

It also depends on the sixth wave's stream F in a way worth stating: a
decomposition is only as good as its percentiles, and a percentile from a lossy
run is a lower bound. Do not start P2 before F has landed, or the first thing it
reports will be a tail that is short by an unknown amount.

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

### Not a source at all, and worth knowing it is not

| | Why it is here | Priority |
|---|---|---|
| **CTF import** — ingest an LTTng-UST stream and merge it into the timeline | Some people have already instrumented their applications in LTTng-UST, mostly in embedded and telecom. Reading their stream gives their application semantics for free. It is a **consumer of one more format, not a source**: `DECISIONS.md` boundary 6 says why LTTng is a peer rather than a backend, and why LTTng-UST in particular can never be an eBPF-readable one — what makes it fast, writing in-line into its own buffer with no trap, is exactly what puts its events out of reach of a BPF program. | Low. Nothing depends on it and no measurement here is worse without it. |
| **A flight recorder** — a permanent circular buffer, snapshotted when a predicate fires | The one class of problem this tool cannot serve at all today: a p99.9 that cannot be reproduced, where a run is one window opened by hand and the interesting moment has to happen inside it. | **Not low, and not here.** It is the run model, not a feature, so it is an open line in `DECISIONS.md` folded into the session-versus-daemon decision it is really part of. Deciding it changes what every collector written in the meantime assumes. |

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

The same three that have held for six waves, and `docs/briefs/README.md` keeps
them beside every brief that proved them:

1. **The unit of ownership is a file set nobody else writes.** Here it is a
   file-name prefix rather than a directory, because a crossing belongs to no
   family: `examples/cross-<name>-*.yml`, `tests/cross_<name>.rs`,
   `docs/scenarios/cross-<name>.md`.
2. **The handoff is a file only that stream can write.** Yours is
   `docs/scenarios/cross-<name>.md`, and it is the deliverable rather than a
   log about one.
3. **A crossing is landed when it says what evidence it has.** `IDENTITY` >
   `KEY` > `WAKEUP` > `TEMPORAL`, on every joined result. A correlation backed
   by a propagated identity and one backed by two things happening at the same
   time are both correlations, and only one of them is evidence.

Read `STATE.md` for where the project is, `docs/scenarios/<family>.md` for what
each family's workloads guarantee, and `examples/allocation-cost.yml` for a join
that works today. Build with `make`: a green suite on an unprivileged binary
says nothing about the half of this that crosses an eBPF source.
