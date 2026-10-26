# Brief — the twenty-four collectors nothing blocks, and the tail nobody is told about

Six work streams. The sixth fan-out, and it divides by **family** again, the way
the first wave did and for the same reason: one directory each, merges that
cannot conflict. Five family streams plus one layer stream that shares no file
with any of them.

The previous five briefs are in `docs/dev/briefs/`, with what each one taught. Read
`docs/dev/STATE.md` for where the project is, `docs/GUIDE.md` for how to run what you are
building, `CLAUDE.md` for the invariants, and `docs/bpf-layer.md` for the
calling conventions.

---

## What changed before this wave, and why it matters to every stream

**The catalog pass has landed.** Twenty-seven entries carry a `bpf:raw_tp` ahead
of their `bpf:tp`, and the two probes that were written and never called are
wired. On the development machine that moved `62 usable here` to `86`, and
`22 out of reach` to `4`.

The consequence for you is not the number, it is the answer you will get:

```console
$ omn mem.reclaim --system -m trace -e hist -d 200ms
error: no collector is written for `mem.reclaim` on backend `bpf:raw_tp` yet
```

Every source below now resolves onto a backend this machine serves and says
*this build has not written it* — exit 3. Before, it said *this machine cannot
serve it* — exit 1 — and you would have had no way to tell a collector you have
not written from a backend you cannot reach. Finding 36 of
`docs/open-findings.md` records the pass; you should not need to touch
`catalog/*.yaml` for an attach path, and if you think you do, say so under
`## Needs from another stream` rather than editing it.

**Three entries were deliberately left**, and a stream that owns one must not
"fix" it in passing:

- `gpu.job` — `drm_run_job` and `drm_sched_process_job` are module tracepoints,
  absent from `vmlinux` BTF. The classic path is the only one it has.
- `io.fs` and `io.readahead` — their requirement is a glob, `tracepoint:ext4:*`
  and `tracepoint:filemap:*`, and a glob is not a name. Converting one means
  choosing which tracepoint the collector attaches to, which is the collector
  author's call. Stream A makes it and writes down why.

---

## The streams

| | Stream | Sources | Owns |
|---|---|---|---|
| **A** | `io` through the block layer | `io.latency`, `.queue`, `.size`, `.pagecache`, `.readahead`, `.writeback` | `src/bpf/io*.rs`, `src/collect/io/**` |
| **B** | `net` through skb and sock | `net.tcp.lifetime`, `.tcp.retransmit`, `.drops`, `.qdisc`, `.softirq` | `src/bpf/net*.rs`, `src/collect/net/**` |
| **C** | `mem` through the allocator | `mem.page_alloc`, `.kmalloc`, `.reclaim`, `.compaction` | `src/bpf/mem*.rs`, `src/collect/mem/**` |
| **D** | `proc` through process events | `proc.syscall`, `.exec`, `.exit`, `.signals` | `src/bpf/proc*.rs`, `src/collect/proc/**` |
| **E** | `sched` contention and interrupts | `sched.lock`, `.futex`, `.irq`, `.numa_balance`, `gpu.job` | `src/bpf/sched*.rs`, `src/collect/sched/**`, `src/collect/gpu/**` |
| **F** | The distribution that does not say it is short | *no source* | `src/emit.rs`, `src/render.rs` |

**Nobody owns `src/bpf/attach.rs`, `insn.rs`, `map.rs`, `btf.rs` or
`syscall.rs`.** Those are the shared layer. A stream needing something from them
writes the request under `## Needs from another stream` in
`docs/open-findings.md`, with the exact signature wanted, and carries on. If two
streams need the same thing, the coordinator lands it once — that is what wave
two learned and what wave three paid for by not doing.

**F shares no file with A–E**, which is why it is in this wave rather than
waiting for the next. It also has no `src/bpf/` and no catalog entry: it is the
one stream that adds no source.

---

## Stream A — `io` through the block layer

> **Mission.** Six sources, and the two that matter most in the project. You own
> `src/bpf/io*.rs` and `src/collect/io/**`.

**`io.latency` and `io.queue` are the point of this wave.** They both join on
`blk:rq@open_ts` with `scoped_eq` — the one match kind the engine implements and
**no source produces**. Landing them turns a quarter of the join engine from
theory into something checked, and it is what phase 4 of the roadmap has been
waiting on since the vertical case was written.

They are also the reason the `blk` key is scoped. A request pointer is recycled
within milliseconds, so `(ptr, open_ts)` is the identity and `ptr` alone is a key
that silently attributes one request's latency to another. That is in the
catalog already; the collector has to honour it, and a scenario has to show that
it does — drive enough I/O to recycle a pointer and check that no completion is
attributed to a request that had already finished.

**`io.fs` and `io.readahead` carry the glob decision.** Pick the tracepoint the
collector actually attaches to, put its name in the catalog entry in place of the
glob, and say in `docs/families/io.md` why that one. `tracepoint:filemap:*` has
several members and they answer different questions; the entry should name the
one the collector reads, not the set it might have.

**Trap.** `io.bytes`, `.iops`, `.merges`, `.inflight` and `.pressure` already
collect through `/proc/diskstats` and are already checked. You are adding
backends *above* them in chains that exist. The scenario that matters is the
backend comparison: where a source now has both an eBPF and a procfs backend,
the two have to agree or the entry has to say what the procfs one degrades —
which it already does, so check the claim rather than writing a new one.

---

## Stream B — `net` through skb and sock

> **Mission.** Five sources, all of which the `net` family already has a key
> discipline for. You own `src/bpf/net*.rs` and `src/collect/net/**`.

`net.tcp.lifetime` joins on `sock:cookie` with `eq` and a 300 s TTL. That is the
key this family is built on, for the reason `docs/scenarios/net.md` records: a
received packet is processed in softirq context, outside any process, so the pid
is not the identity and the socket cookie is.

**`net.drops` carries the drop reason** that recent kernels attach to
`skb:kfree_skb`, and it is the source that makes the family's headline honest:
`net.iface` counts frames the device lost and cannot see a frame dropped inside
a queueing discipline, which is what `net.qdisc` is for. Landing both is what
lets a reader tell those two apart.

**Trap.** `net.capture` needs `CAP_NET_RAW`, which the build does not grant, and
that is a decision deliberately left open rather than an omission — do not grant
it to make a scenario run. `net.xdp` is not in this wave: an XDP program is a
different program type and the only one here that can drop packets rather than
observe them.

---

## Stream C — `mem` through the allocator

> **Mission.** Four sources, and the family with the most collectors already. You
> own `src/bpf/mem*.rs` and `src/collect/mem/**`.

`mem.reclaim` is the one worth landing first: direct reclaim is the kernel doing
the allocator's work inside the allocating thread, one of the clearest memory
problems there is, and until now it has been the repository's worked example of
*a source that is vocabulary and not a measurement*. Two sessions and a scenario
document point at it saying exactly that; when it collects, update them.

**Trap, and it is this family's own.** `mem.page_alloc` and `mem.kmalloc` fire
at rates no other source here approaches. Aggregate in the kernel or do not
land them: `src/bpf/runqlat.rs` is the worked example — `(key, bucket) -> count`
with an unrolled log2 search, and `emit::WEIGHT_ATTR` on each row so a bucket
standing for four hundred measurements counts as four hundred. A `trace` mode
that streams every `mm_page_alloc` to userspace will bury the ring buffer, and
the loss counter will tell you so honestly, which is not the same as working.

Coordinate with stream F on `WEIGHT_ATTR`: you are its heaviest new user and F
owns the file it lives in.

---

## Stream D — `proc` through process events

> **Mission.** Four sources, and the family with the fewest collectors — two of
> six. You own `src/bpf/proc*.rs` and `src/collect/proc/**`.

`proc.exec` and `proc.exit` are the cheap pair and they unlock more than they
look: a launched target's lifecycle is currently inferred by polling `/proc`,
and three defects in `docs/scenarios/proc.md` are about the races that creates.
An exec event is the moment the child stops being this tool's own image — which
`docs/dev/STATE.md` records as a trap hit three times in three different shapes.

**`proc.syscall` is the expensive one.** `raw_syscalls:sys_enter` fires on every
syscall of every process, and the catalog says so: the entry declares
`degraded: "raw_syscalls fires on everything: filter by syscall number
in-program"`. That degradation is a promise about where the filter runs — honour
it in the program, and make the resolved file say the filter was pushed down,
because a filter that is reported as pushed down and applied in userspace is the
defect `docs/DECISIONS.md` calls the worst shape a bug takes here.

---

## Stream E — `sched` contention and interrupts, and `gpu.job`

> **Mission.** Four `sched` sources whose catalog entries were written to the raw
> path in wave three, plus the one collector in this wave that cannot be
> exercised here. You own `src/bpf/sched*.rs`, `src/collect/sched/**` and
> `src/collect/gpu/**`.

`sched.lock` and `sched.futex` are why the kernel floor is 6.1:
`lock:contention_begin` and `lock:contention_end` exist from there, and
`docs/DECISIONS.md` chose that floor specifically so these two would not need a futex
and uprobe fallback nobody would exercise. They are the payoff of a decision
already paid for.

**`gpu.job` is the exception in this wave and must be labelled as one.** Its two
tracepoints are in the DRM scheduler module and absent from `vmlinux` BTF, so it
is the only collector here written against the classic path — and this machine's
proprietary NVIDIA driver does not publish them at all. So it is a collector
that will be written here and exercised elsewhere. Say that in
`docs/families/gpu.md`, make the scenario skip loudly with the reason, and do
not assert that it works. A fix you cannot exercise is a claim.

---

## Stream F — the distribution that does not say it is short

> **Mission.** One defect, finding 37 of `docs/open-findings.md`, and it is the
> class this project exists to prevent: the tool telling a reader something that
> is not true, by omission. You own `src/emit.rs` and `src/render.rs`.

A `hist`, a `summary` or any percentile built from a run that dropped events or
was throttled is rendered exactly like one built from a complete run. The events
it is missing are **the slow ones** — whatever stalled the machine is what
stopped the measuring — so every percentile from a lossy run is a lower bound,
and nothing says so. A reader takes a p99 off it with nothing beside that number
saying the distribution is short; the loss line is printed, and correct, and
several lines away, and phrased as a count of events rather than as a statement
about the shape.

**What is already right, and you should not change it.** `poll` cannot produce
`hist` at all — `docs/CATALOG.md` §12 refuses it, because a histogram of a polled
gauge is a histogram of when the poller looked. That is the same argument under
another name and it closes the largest opening there was. The losses themselves
are counted and surfaced: `dropped`, `throttled`, `late_ticks`, `failed_reads`,
`undecoded`, and `loss_headline` prints all five. The invariant holds. You are
adding the step after it.

**What to build.**

- A `hist`, `summary` or percentile from a run with `dropped` or `throttled`
  above zero is marked **beside the value, not above it**, with what fraction of
  events is unaccounted for.
- The mark says which way the bias runs, because it only runs one way: **every
  percentile is a lower bound**, never an estimate.
- Where the number of missing events is unknown — the throttled case — it says
  that instead of a fraction. `docs/DECISIONS.md` already chose that answer for the
  loss line: *"how many events happened inside that while is not recorded
  anywhere"*, so a throttled run says it cannot tell how much it lost. Say the
  same thing here rather than inventing an estimate.

**Traps, and there are three.**

- `format.rs` declares what each format can carry and whether its encoder
  exists, and a test crosses the claim against reality because a merge once
  reverted an encoder and left the claim behind. Every format that can carry a
  `hist` has to carry the mark too, or say it cannot.
- `emit::WEIGHT_ATTR` exists so a kernel-built bucket standing for four hundred
  measurements counts as four hundred. **Stream C is about to become its
  heaviest user.** A weighted bucket and a lossy run are different facts and
  must not be conflated: a bucket weighted 400 is complete, a run that dropped
  400 events is not.
- Findings 17 and 18 record two claims about kernel-side histograms that are
  now false. Read them before touching the weighting path.

**Your scenarios are not a family's.** There is no workload here to choose a
property. Substitute the losses instead: the `io` scenarios already show how,
by substituting a `/proc/diskstats` the test wrote. A run whose `Losses` you set
by hand, rendered in every format that carries a distribution, and checked for
the mark — plus the negative, which matters as much: **a clean run must gain
nothing**, or the mark becomes noise and readers learn to ignore it.

---

## Rules

The three that have held for six waves:

1. **The unit of ownership is a file set nobody else writes.** Here it is one
   family directory plus that family's own files under `src/bpf/`, and for F two
   files at the top of `src/`. The shared eBPF layer belongs to nobody.
2. **The handoff is a file only that stream can write.** Yours is
   `docs/families/<family>.md`, extended rather than replaced; F's is
   `docs/families/emit.md`, new.
3. **A source is landed when its scenarios are.** Four classes, defined in
   `docs/dev/briefs/wave-4-scenarios.md` and explained for a reader in `docs/GUIDE.md`:
   ground truth, the capability sweep, the loss path, the refusal. A collector
   without them is half the work.

A stream edits only its own paths. Needing something from another stream's files
is not a reason to edit them: write it under `## Needs from another stream` in
`docs/open-findings.md` with the exact signature wanted, and carry on.

**Build with `make`, not `cargo build`.** Every one of these needs `CAP_BPF` and
`CAP_PERFMON`, and every relink drops them; a green suite on an unprivileged
binary says nothing about any of this. `make check` clean before you hand over.

One branch per stream, `feat/<family>-bpf` and `feat/emit-loss`, made by
`scripts/spawn.sh`. Commit there; the coordinator merges.

**A last word on tests, because this wave will hit it.** Three of the last four
sessions on this project lost time to a test that asserted what the machine was
doing rather than what the tool did — a tracefs probe, a shared GPU, a namespace
given 250 ms to build. Findings 11 and 35 have them. Where your scenario depends
on something the machine may or may not provide, **assert the separation rather
than a threshold, and where the property cannot be created, skip loudly with the
numbers that show why.** A scenario that states its limit is worth more than one
that hides it.
