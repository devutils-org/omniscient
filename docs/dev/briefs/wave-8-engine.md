# Brief — the engine the crossings asked for, and the two attach kinds left

Four work streams. The eighth fan-out, and it divides **by layer**: one stream
rewriting the join engine, one the run model, and two writing the attach kinds
that eight sources have been waiting on since the fifth wave.

This wave touches `src/` everywhere, so the lock is a **directory or a named
file list** rather than the file-name prefix the seventh wave used. Read
`docs/dev/STATE.md` for where the project is, `docs/GUIDE.md` for how to run what you are
building, `CLAUDE.md` for the invariants, `docs/bpf-layer.md` for the eBPF
calling conventions, and `docs/scenarios/cross-*.md` for what the crossings
measured — three of the four streams below exist because of something one of
them could not say.

The seven earlier briefs are in `docs/dev/briefs/`, with what each one taught.

---

## What changed before this wave, and why it matters to every stream

**The crossings landed and they agree with each other.** Four streams asked one
question each, and three of them independently asked for the same missing piece
of the engine — a match kind whose evidence is `TEMPORAL` by construction. When
three streams that never spoke ask for the same thing, it is a gap in the model
rather than a preference, and stream **J** is that.

**The shared eBPF layer is clean.** The sixth wave's twenty requests were landed
in the seventh (`docs/families/shared-layer.md`): `Btf::member_offset` descends
anonymous members, `Btf::enum_variant` reads the kernel's own names,
`insn::helper` names the helpers four families had declared locally, one copy of
each instruction sequence replaced up to eight, and `attach.rs` publishes the
tracefs root and the `format` parser. Streams **T** and **U** are the first to
build on that rather than around it.

**The catalog means what it says**, entry by entry, as of the pass that closed
D2. If you find an entry offering something no collector serves, that is a
finding and not a nuisance — write it down.

---

## The streams

| | Stream | The work | Owns |
|---|---|---|---|
| **J** | the join engine and the decomposition | a `between` match kind, a `decompose:` block, wakeup chains, two refusals | `src/join.rs`, `src/request.rs`, `src/encode.rs`, `src/session.rs`, the match kinds in `src/catalog.rs`, `tests/join_engine.rs`, `examples/join-*.yml` |
| **R** | the run model | several targets in one run, the launched program's lifetime, one shim stream per launch | `src/collect/mod.rs`, `src/collect/launch.rs`, `src/collect/task.rs`, `src/collect/mem/heap/**`, `tests/runner.rs` |
| **T** | fentry, and the five sources behind it | `BPF_PROG_TYPE_TRACING` with an `attach_btf_id`, then `io.syscall`, `io.fs`, `io.fsync`, `net.tcp.connect`, `net.socket` | `src/bpf/attach.rs`, `src/bpf/fentry.rs`, `src/bpf/io*.rs`, `src/bpf/net*.rs`, `src/collect/io/**`, `src/collect/net/**`, `catalog/io.yaml`, `catalog/net.yaml`, `examples/io-*.yml`, `examples/net-*.yml`, `tests/scenario_io.rs`, `tests/scenario_net.rs`, `docs/families/io.md`, `docs/families/net.md` |
| **U** | USDT, and the `runtime` family | `.note.stapsdt`, the semaphore protocol, then a uprobe: `runtime.gc`, `runtime.alloc`, `runtime.custom` | `src/bpf/usdt.rs`, `src/collect/runtime/**`, `catalog/runtime.yaml`, `examples/runtime-*.yml`, `tests/scenario_runtime.rs`, `docs/families/runtime.md` |

**T owns `src/bpf/attach.rs`** because fentry is an attach kind and that is where
attach kinds live. **U reads it and does not write it**: the uprobe half of USDT
already exists there, and what U adds — parsing an ELF note section — is not an
attach kind and belongs in a file of its own. If U finds it needs a change in
`attach.rs`, that is a request, not an edit.

**Nobody owns `src/emit.rs`, `src/catalog.rs` beyond the match kinds,
`src/validate.rs`, `src/bpf/btf.rs`, `insn.rs`, `map.rs` or `syscall.rs`.** A
stream needing something from any of them writes the request under
`## Needs from another stream` in `docs/open-findings.md`, under a heading
naming the stream, with the exact signature wanted, and carries on. That file is
**append-only per stream**: four merges of the seventh wave produced four
conflicts and every one of them was there.

**The coordinator owns `examples/README.md`, `docs/GUIDE.md`, `docs/dev/STATE.md`,
`README.md`, `docs/DECISIONS.md` and `docs/ROADMAP.md`.** Do not edit them. The two
session indexes are held to the sessions on disk by a test now, in both
directions, so a session you add without a row is a red suite rather than a
silent gap — and that test is the coordinator's to satisfy at the merge.

---

## Stream J — the join engine, and the decomposition

> **Mission.** Four of the seventh wave's crossings computed their answer in a
> test because the engine cannot express it. Make the engine express it.

**1. A `between` match kind, and it is the one three streams asked for.**

```rust
// catalog.rs
pub enum MatchKind { Interval, Eq, ScopedEq, Wakeup, Between }

// join.rs
/// `between`: each `to` reading's change since the previous reading with the
/// same `via` key is credited to the `from` states that opened or closed in
/// that interval. Always `Evidence::Temporal`; `check` refuses it when `to` is
/// not a `poll` or `snapshot` source, since a traced event has no previous
/// reading to differ from.
fn between(rule: &Rule, events: &[&Event]) -> Result<Correlation, JoinError>;
```

The point is the evidence level, not the arithmetic. Today the only rule that
validates between an allocation and a resident size is `eq` on `task`, and it
reports **`KEY`** — which overstates what crediting-by-recency knows. A
correlation backed by a propagated identity and one backed by two things moving
at the same time are both correlations, and the ladder exists so a reader can
tell them apart. `docs/scenarios/cross-memory.md` D1 is the measurement.

**2. A `decompose:` block.** `docs/open-findings.md` carries the wanted shape
from two streams, and they agree: a `whole`, a list of `parts`, and a **signed
residue that is never rounded away** — negative is time counted twice, positive
is a state no row covers. Each row carries its kind (`Sampled`, `Polled`,
`Traced`), its evidence, its resolution, and its `Shortfall` if the run was
lossy. A sampled profile and a traced duration are not the same kind of number,
and the table says which is which.

Refuse at validation when a part's source is not in the session, or carries a
different task than the whole.

**3. Wakeup attributions grouped by chain**, asked by P2 and P3. A wakeup join
prints its heaviest chains already; what is missing is the same grouping as a
ranking a reader can take a decision from.

**4. Two refusals.** `eq` on a scoped key should be refused or rated lower — a
bare pointer is exactly the key `scoped_eq` exists to correct. And a wakeup join
onto a *drained total* matches nothing and says it attributed zero, which reads
as a workload that did nothing: `check` can see that the `to` source is an
aggregate before the run.

**Trap.** `src/encode.rs` renders results and `src/render.rs` renders requests,
listings and refusals — the sixth wave's brief named the wrong one and cost a
stream an hour. And `format.rs` declares what each format can carry, with a test
crossing the claim against reality: a decomposition that only the text encoder
can render has to say so there.

---

## Stream R — the run model

> **Mission.** Three things the crossings could not do in one run, all of them
> in the runner rather than in any collector.

**1. More than one target in one run**, which is the one that matters.
`CollectError::MixedTargets` refuses any session whose requests name two
targets, and **every confounder check has exactly that shape**: the machine's
state is `system` or `cpu`, the measurement is `pid` or `cgroup`. The machine
crossing had to run a second `omn` beside the first, and measured what that
costs: a `cpu`-target counter also counts whatever else ran on that core, which
under load was **70 % of it**. The reclaim crossing read the machine's PSI
instead of the cgroup's, and the machine's value diluted a 12.5 ms stall to
6.0 ms.

`docs/open-findings.md` has the minimum that would serve — any number of machine
targets beside at most one workload target, with only the workload target
driving the launch and the stop condition — and the two functions it needs.

**2. The launched program's lifetime.** Finding 4 of the compute crossing.

**3. One shim stream per launch.** Two shim-backed sources on one launch
disagree about `OMN_HEAP_FD` and the run is refused before it starts, which is
honest, and it exits 2 where `docs/DECISIONS.md` makes it 3: the request means
something and the build cannot serve it. Whether the fix is one pipe shared or a
better refusal is your call; say which in your log.

**Trap.** Everything here changes what a run *is*, which is what
`docs/DECISIONS.md`'s open line on session-versus-daemon is about. Do not settle that
line in passing: if a change would make a run something other than one window
and one document, write it down instead.

---

## Stream T — fentry, and the five sources behind it

> **Mission.** The last attach kind worth having, and the layer above the block
> one that this project has been one mechanism away from for three waves.

`BPF_PROG_TYPE_TRACING` with an `attach_btf_id`, which is why it is typed:
cheaper per call than a kprobe, and it fails to load rather than reading the
wrong field. The BTF this needs is already published —
`Btf::tracepoint_signature` is the pattern to copy for reading a function's
prototype, and `member_offset` descends anonymous members since the seventh
wave.

Five sources: `io.syscall`, `io.fs`, `io.fsync`, `net.tcp.connect`,
`net.socket`. **`io.fs`'s glob is yours to decide**, and it is the decision the
seventh wave deliberately did not make: `tracepoint:ext4:*` was left because
converting it means choosing which filesystem `io.fs` is about, and a machine
with btrfs and xfs mounted would get the ext4 answer or nothing. Its first
backend is `bpf:fentry` on `vfs_read`, which is the right answer and is now a
mechanism you have. Name what you attach to and say why in `docs/families/io.md`.

**Trap.** `io.syscall` and `net.socket` sit on paths that fire on everything.
Aggregate in the kernel or do not land them — `src/bpf/runqlat.rs` is the worked
example and `insn::push_log2_bucket` is now one shared sequence, so a histogram
you build agrees with the emit layer's by construction rather than by luck.

**And the five sources are not the deliverable on their own.** A source is
landed when its scenarios are: ground truth, the capability sweep, the loss
path, the refusal.

---

## Stream U — USDT, and the `runtime` family

> **Mission.** The family with one collector of four, and the mechanism that
> unlocks the other three.

`.note.stapsdt` in the target's ELF, the semaphore protocol, then a uprobe —
and the uprobe half exists. What is missing is reading the notes: a base
address, a semaphore address, a provider and a name, plus the argument
descriptors that say where each argument lives at the probe site.

**The measurement that frames this**, from `docs/open-findings.md`: a preload
shim costs **132 ns** against a uprobe's **740 ns** for the same event. USDT is
not the cheap path; it is the path that reaches a program you did not launch and
cannot preload into, and one that names its own events rather than having them
guessed. Say that in `docs/families/runtime.md` rather than letting a reader
assume a probe is free.

**Trap, and it is this family's own.** A USDT probe in a program that is not
running is not a refusal you can test on this machine by accident — pick a
target whose notes you can rely on (`libc`'s own, a Python built with them), and
where you cannot, skip loudly with what the ELF actually contained.

---

## Rules

The three that have held for eight waves:

1. **The unit of ownership is a file set nobody else writes.** Here it is a
   named list per stream, because this wave is layers rather than families.
2. **The handoff is a file only that stream can write**, and it is the
   deliverable rather than a log about one: `docs/families/join.md` for J,
   `docs/families/runner.md` for R, `docs/families/io.md` and `net.md` for T,
   `docs/families/runtime.md` for U.
3. **A source is landed when its scenarios are**, and an engine change when the
   evidence it produces is checked. `IDENTITY` > `KEY` > `WAKEUP` > `TEMPORAL`,
   on every joined result — J is the stream that decides what the last of those
   means, so J's own tests are the ones that say it is not a synonym for the
   others.

A stream edits only its own paths. Needing something from another stream's files
is not a reason to edit them: write it under `## Needs from another stream` in
`docs/open-findings.md`, under a heading naming your stream, with the exact
signature wanted, and carry on. If two streams need the same thing, the
coordinator lands it once — that is what the second wave learned, what the third
paid for, and what the seventh finally did.

**Build with `make`, not `cargo build`.** The preload shim is a workspace member
nothing depends on, and `CAP_BPF` and `CAP_PERFMON` are file capabilities every
relink drops. Both failures are silent: the tool reports fewer sources as
available, which reads like the machine changed rather than the binary.
`make check` clean before you hand over.

One branch per stream, made by `scripts/spawn.sh`. Commit there; the coordinator
merges.

**A last word on tests.** `docs/scenarios/io.md` claims `scenario_io` never
flaps and it does — two failures in three runs under load, and one run in three
on an untouched tree that reported `0 bytes traced for the 16777216 the workload
read`. If you see a red suite on this machine, check it against that list before
assuming it is yours, and add to the list if it is not. Assert the separation
rather than a calibrated threshold, carry the reference's own resolution as
slack, and where the property cannot be created, skip loudly with the numbers
that show why.
