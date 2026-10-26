# Brief — the vertical case, and the six sources nothing blocks

Four work streams. The ninth fan-out: one stream on the milestone `docs/ROADMAP.md`
calls *the real one*, and three on breadth that needs no new mechanism.

Read `CLAUDE.md` first — **including "How much to test, and when", which this
wave is held to** — then `docs/dev/STATE.md`, then your section here, then
`docs/contributing.md`. The eight earlier briefs are in `docs/dev/briefs/`.

---

## Before anything else: how you test

The full suite is about 1600 tests and takes 5 to 10 minutes, because some 270
scenarios measure the real machine. Four streams running it by reflex on one
machine is what made the last wave slow, and it bought nothing: on a loaded
machine almost every full run trips a *different* flaky test.

So, in this wave:

- **While you work**: run only your own module and your own scenario file —
  `cargo test --lib <module>` and `cargo test --test <your file>`. Never
  `make check` while iterating.
- **Before each commit**: one `make check`. If it is red, run
  `cargo test --workspace --no-fail-fast` once to see every failure together.
- **A red test outside your paths**: look it up in the flaky tally
  (`docs/open-findings.md` findings 41–46, `docs/scenarios/io.md`). If it is
  there, rerun it alone once and move on. Do not rerun the suite for it, and do
  not try to fix it — it is not yours.
- **`make`** once per build, before eBPF tests, because a relink drops the
  capabilities. Not once per test.

A stream that runs the full suite five times per commit will be the slowest of
the four and no safer for it.

---

## What changed before this wave

**Every mechanism the vertical case needs exists except one.** `fentry` reaches
the syscall and VFS boundaries (`io.syscall`, `io.fs`), `scoped_eq` joins a
request's submission to its completion at 99.9 %, the `wakeup` join says what
released a block, `sched.offcpu` separates the queue wait from the block, and
the engine decomposes a whole into parts with a signed residue — one task's wall
clock accounts for its life to within 1.58 ms. What is missing is the one thing
that lets a *request* rather than a *task* be the whole: a request identity that
travels from where the request enters to where it touches the disk.

**`KeyValue::Req` exists and nothing emits it.** `io.latency` even declares
`req` among its keys. That is a promise stream V makes true.

**Eleven sources have no collector, and six of them need none of the missing
hardware.** `io.pagecache` and `io.readahead` resolve onto a raw tracepoint this
machine serves and wait for a collector; `sched.mutex` and `gpu.api` are the
preload shim pointed at something other than `malloc`; `mem.latency` and
`mem.sharing` have a `pmu:ibs` backend and this machine is an AMD Zen 2, which
has IBS.

---

## The streams

| | Stream | Delivers | Owns |
|---|---|---|---|
| **V** | the vertical case | one request's latency, decomposed boundary by boundary, against a server with injected delays | `src/bpf/map.rs`, `src/bpf/syscall.rs`, `src/bpf/io.rs`, `src/bpf/request*.rs` (new), `src/collect/io/block.rs`, `src/collect/net/request*.rs` (new), `catalog/net.yaml`, `tests/vertical.rs`, `examples/vertical-*.yml`, `docs/scenarios/vertical.md` |
| **P** | the page cache | `io.pagecache`, `io.readahead` | `src/bpf/io_pagecache*.rs` (new), `src/collect/io/**` **except `block.rs`**, `catalog/io.yaml`, `examples/io-*.yml`, `tests/scenario_io.rs`, `docs/families/io.md` |
| **S** | the shim, pointed elsewhere | `sched.mutex`, `gpu.api` | `shim/**`, `src/collect/mem/heap/**`, `src/collect/sched/**`, `src/collect/gpu/**`, `catalog/sched.yaml`, `catalog/gpu.yaml`, `examples/sched-*.yml`, `examples/gpu-*.yml`, `tests/scenario_sched.rs`, `tests/scenario_gpu.rs`, `docs/families/sched.md`, `docs/families/gpu.md` |
| **B** | memory by IBS | `mem.latency`, `mem.sharing` | `src/perf.rs`, `src/collect/mem/**` **except `heap/**`**, `catalog/mem.yaml`, `examples/mem-*.yml`, `tests/scenario_mem.rs`, `docs/families/mem.md` |

**Two splits to respect, because they cut a directory in two.** P owns
`src/collect/io/` except `block.rs`, which is V's: the block programs are where a
request's identity is read, and P's two sources are new files beside them. And B
owns `src/collect/mem/` except `heap/`, which is S's: the shim's stream is read
there, and S is the one extending what the shim sends.

**Nobody owns** `src/join.rs`, `src/emit.rs`, `src/encode.rs`, `src/request.rs`,
`src/catalog.rs`, `src/validate.rs`, `src/bpf/btf.rs`, `insn.rs` or `attach.rs`.
A stream that needs one writes the request under `## Needs from another stream`
in `docs/open-findings.md`, **under a heading naming the stream**, with the exact
signature, and carries on. That file is append-only per stream.

**The coordinator owns** `examples/README.md`, `docs/GUIDE.md`, `docs/dev/STATE.md`,
`README.md`, `docs/DECISIONS.md` and `docs/ROADMAP.md`. The session-index test will be red
on your branch for the sessions you add. **That is expected** — list the rows in
your handoff and the coordinator adds them at the merge. Do not edit the indexes.

---

## Stream V — the vertical case

> **Mission.** `docs/ROADMAP.md` phase 4: *an HTTP request that reads a file and sends
> it back*, decomposed across its boundaries, each line carrying its evidence,
> and checked against a server whose delays the test injected.

**The deliverable is the tree in the roadmap**, produced by the tool:

```
req:a3f1   total 412 ms
├── net_rx              0.4 ms   KEY
├── sched_wait          8.1 ms   WAKEUP
├── cpu_on             12.3 ms   IDENTITY
├── lock_wait          41.2 ms   WAKEUP     → holder tid=8821
├── block_io          347.0 ms   IDENTITY   → 3 rq, dev 259:0
└── net_tx              2.8 ms   KEY
```

**The one new mechanism is `bpf_task_storage`**, and the roadmap is explicit about
why: *step 6 is the crux. Do not join `bio → inode → file → fd → task` (fragile,
four inferred joins). Read the `req:id` from the current task's storage at
`block_rq_issue`: a read is submitted in the reader's context. One propagated key
replaces four inferences.* So: a `BPF_MAP_TYPE_TASK_STORAGE` map in `map.rs`, a
program at request entry that writes an id into the current task's storage, and
the block program in `io.rs` reading it at issue and putting `req:<id>` on the
event. That is what makes `block_io` **IDENTITY** rather than KEY.

**Where a request is born is your decision, and it is the important one.** Two
honest options: derive it from the connection (the socket cookie and a sequence
number, at the first receive), or let the server declare it through a USDT probe
— `runtime.custom` attaches to one since the eighth wave. The first needs nothing
from the program and guesses where a request starts; the second is exact and
needs the program's cooperation. Write down which, and why, in
`docs/scenarios/vertical.md`.

**The ground truth is the roadmap's exit criterion**, and it has to hold on this
machine without root: a synthetic server with known injected delays — a `usleep`
under a lock for `lock_wait`, a cold read (`POSIX_FADV_DONTNEED`, as the I/O
crossing learned) for `block_io` — decomposed so that **the segments sum to the
client's measured latency within 10 %**, and each injected delay lands in the
row it was injected into. `tc netem` and `dm-delay` need root; where you would
have used them, say so and skip loudly.

`decompose:` already takes a `by:` key. If `by: req` works as it stands, use it;
if it does not, that is a request to whoever owns `src/join.rs`, not an edit.

---

## Stream P — the page cache

> **Mission.** `io.pagecache` and `io.readahead`, which have waited only on a
> collector since the seventh wave landed the BTF walk into `folio.mapping`.

`io.readahead`'s catalog entry names two tracepoints — `mm_filemap_get_pages` is
the denominator and `mm_filemap_add_to_page_cache` the numerator of readahead
effectiveness — and stream A's reasoning for choosing those two of five is in
`docs/open-findings.md`. `io.pagecache` keys by `task` and `file`, the file being
`mapping->host->i_ino` and `->i_sb->s_dev`; every step of that chain is reachable
through `Btf::member_offset` now, anonymous union included.

**Ground truth**: a file of known size read cold and then warm. A cold read must
add to the page cache what it read, a warm one nothing; a sequential read must
show readahead ahead of what was asked, a random one must not. Aggregate in the
kernel — these fire on every page — and give every row program a stack-region
test and a byte-coverage test before anything else: finding 46 is the file that
had neither.

---

## Stream S — the shim, pointed elsewhere

> **Mission.** `sched.mutex` and `gpu.api`, through the `LD_PRELOAD` shim that
> already interposes `malloc`.

The shim costs **132 ns** per event against a uprobe's **740 ns** for the same
one, and that is why it is the right tool for `pthread_mutex_lock`, which fires
far more often than an allocation. Since the eighth wave a launch carries **one**
shim stream that every preload collector reads; extend what the shim sends
rather than opening a second pipe — two pipes for one program is the D3 defect
the run model stream fixed.

`sched.mutex` is a user-space lock wait, beside `sched.lock` (kernel) and
`sched.futex` (the syscall under a contended mutex). Say what each sees that the
others do not: an uncontended mutex never reaches the kernel, and a futex wait is
not always a mutex. **Ground truth**: two threads contending a mutex for a
known number of acquisitions with a known hold time.

`gpu.api` is host-side submission latency. **This machine's proprietary NVIDIA
driver may not make GL/Vulkan entry points reachable in a headless test**, and
that is fine: write the collector, prove the interposition on a symbol you can
call, and skip loudly with what the machine offered. A collector written here and
exercised elsewhere says so — `gpu.job` is the precedent.

---

## Stream B — memory by IBS

> **Mission.** `mem.latency` and `mem.sharing` through AMD IBS, which this machine
> has and which the catalog already names as `pmu:ibs`.

IBS op sampling tags a micro-op and reports, among other things, its **data
address and its load latency** — which is the thing a normal PMU sample cannot
say: where the access went and how long it waited. That is `mem.latency`. Two
tasks touching the same line is `mem.sharing`.

This is `perf_event_open` with a dynamic PMU type read from
`/sys/bus/event_source/devices/ibs_op/type`, not eBPF, and it is the first
source here to read the sample's data address — so `src/perf.rs` is yours.
**Ground truth**: a walk whose stride and working set are chosen so that the
latency is known to land in L1 on one pass and in memory on another, and two
threads writing one cache line against two threads writing two. The `mem`
family's scenarios already turn a 64 MiB shuffled walk against a 4 KiB one into
a measurement — read `docs/scenarios/mem.md` before writing a new workload.

---

## Rules

The three that have held for eight waves:

1. **The unit of ownership is a file set nobody else writes.** The table above;
   the two directory splits are the ones to be careful with.
2. **The handoff is a file only that stream can write**: V's is
   `docs/scenarios/vertical.md`, and P, S and B extend their family documents.
   End it with **the index rows for every session you added**, ready to paste.
3. **A source is landed when its scenarios are**: ground truth, the capability
   sweep, the loss path, the refusal. For V, the vertical case is landed when
   the injected delays land in their rows and the segments sum to the client's
   latency.

**Build with `make`, not `cargo build`** — the preload shim and the capabilities
both fail silently otherwise. One branch per stream, made by `scripts/spawn.sh`.
Commit there; the coordinator merges.
