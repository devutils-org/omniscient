# Brief — the crossings, the debt underneath them, and the tail nobody marked

Six work streams. The seventh fan-out, and it divides **by question rather than
by family**: four crossings, each asking something no single family can answer,
plus two streams that clear what the sixth wave left behind.

The shape of the four crossings was settled in `BRIEF_NEXT.md` before this wave
was dispatched, which is the habit that has held for four waves. The two other
streams are new, and both exist because the sixth wave's merge left work that
belongs to nobody.

Read `docs/dev/STATE.md` for where the project is, `docs/GUIDE.md` for how to run what you are
building, `CLAUDE.md` for the invariants, `docs/scenarios/<family>.md` for what
each family's workloads guarantee, and `examples/allocation-cost.yml` for a join
that works today. The six earlier briefs are in `docs/dev/briefs/`, with what each
one taught.

---

## What changed before this wave, and why it matters to every stream

**The sixth wave closed the two things the crossings were waiting on.**

`scoped_eq` has a producer: `io.queue -> io.latency` joins per request at
99.9 %, keyed on `(pointer, open_ts)` because the kernel recycles a request
pointer within milliseconds. P3 was written as *blocked on stream A*, and it is
not blocked any more.

A distribution now says when it is short: `emit::Shortfall` rides on
`SeriesHist` and `SeriesSummary`, so a percentile from a run that dropped or was
throttled is marked as a lower bound rather than rendered like a measurement.
P2 was written as *do not start before F lands*, and F has landed. Every
crossing in this wave reads percentiles, and every percentile it reads now
carries what it is missing.

**And it left a debt that nobody has paid.** Five streams wrote requests under
`## Needs from another stream` in `docs/open-findings.md` — roughly twenty of
them, three of which were asked for twice by two families independently — and
not one has been landed. `Btf::member_offset` still cannot descend into an
anonymous member, `insn::helper` still has no `bpf_get_current_comm`, no
`bpf_get_current_cgroup_id` and no bitwise `and`, and six `sched` entries still
declare `cgroup: bpf_only` against six collectors that refuse a cgroup by name.
That is stream S, and it is the only work in this wave that blocks a later one:
**fentry and USDT both need `src/bpf/attach.rs`**, and neither can start until S
has finished rewriting it.

---

## The streams

| | Stream | The question, or the defect | Owns |
|---|---|---|---|
| **P1** | Memory, end to end | Which allocation cost memory, and did it stay? | `examples/cross-memory-*.yml`, `tests/cross_memory.rs`, `docs/scenarios/cross-memory.md` |
| **P2** | Compute, end to end | Where did the time go — on cpu, or waiting? | `examples/cross-compute-*.yml`, `tests/cross_compute.rs`, `docs/scenarios/cross-compute.md` |
| **P3** | The I/O path | What did one read cost, from syscall to platter? | `examples/cross-io-*.yml`, `tests/cross_io.rs`, `docs/scenarios/cross-io.md` |
| **P4** | Crossed families | Does the machine's own state explain the measurement? | `examples/cross-machine-*.yml`, `tests/cross_machine.rs`, `docs/scenarios/cross-machine.md` |
| **S** | The shared layer nobody owns | Twenty requests, three of them asked twice | `src/bpf/attach.rs`, `btf.rs`, `insn.rs`, `map.rs`, `syscall.rs`, `mod.rs`, `catalog/*.yaml`, `docs/families/shared-layer.md` |
| **E** | `rate`, `topk` and `stack` are short in the same way | Finding 37, widened | `src/emit.rs`, `src/encode.rs`, `tests/aggregation.rs`, `tests/encoders.rs`, `docs/families/emit.md` |

**P1 to P4 touch no file under `src/`.** That is what lets S and E run beside
them: a crossing is a session, a test and a document, and if one of them needs a
change in the engine to express what it wants, that change is a request in
`docs/open-findings.md` and not an edit.

**S is taken by the coordinator**, because three of its requests came from two
families each and landing those twice is exactly what the second wave learned
not to do.

### Two seams, named rather than discovered

Finding 39's lesson from the sixth wave was *name the seam, or it gets
discovered*. This wave has two:

- **`examples/README.md` and `docs/GUIDE.md` are the session indexes, and they belong
  to the coordinator.** Every crossing stream writes sessions; none of them
  edits either index. After the sixth wave both were twenty-eight sessions
  short, precisely because every stream wrote sessions and no stream owned the
  list. The coordinator regenerates both at the merge — and lands the test that
  holds them to the sessions on disk, which `8ec62ec` said existed and which
  **no test in `tests/` actually performs**.
- **`docs/open-findings.md` is append-only, per stream, at the end.** Wave three
  had six streams append to one findings file and produced six conflicts on it.
  Write your section under a heading that names your stream, and never edit
  another stream's.

---

## Stream P1 — memory, end to end

> **Mission.** `heap.alloc -> mem.faults` already works and is the project's
> first correlation. Take it the rest of the way: `-> mem.rss` and
> `heap.retained`, so a reader can follow one allocation from the call that made
> it to the resident pages it still holds at the end.

The chain is an interval join onto a poll and a snapshot, which is the first
time this repository crosses those two shapes in one session. `mem.rss` is
sampled on a tick and an allocation is an instant: what you can honestly say is
that residency *after* a range opened is consistent with it, not that this range
caused those bytes. Say which one your document claims.

**The trap is already recorded and it is this family's own.** The first
correlation reported ~515 faults for an 8 MiB allocation where 4 KiB pages would
need 2048, and the collector was right: with multi-size transparent huge pages
the fault count is nearly flat across allocation sizes. A derived "resident
bytes" column computed from `faults x page size` was wrong by a factor of four
and was deleted. **Do not reintroduce it under another name.** If your crossing
wants bytes, take them from `mem.rss`, which measures bytes, and say so.

`docs/DECISIONS.md` has the whole episode under "What the measurement taught".

---

## Stream P2 — compute, end to end, and where phase 10 starts

> **Mission.** `cpu.profile` beside `sched.offcpu`, by task, with
> `sched.wakeup -> sched.offcpu` for what released each block. One thread's wall
> clock, split across on-cpu, runnable-and-waiting, and blocked-on-what.

**This is thread state analysis for one task, and `docs/ROADMAP.md` phase 10 puts it
ahead of USE** for a reason worth carrying into your document: it answers *why
is this slow*, which is the question people arrive with, where USE answers
*what is saturated*. Your three inputs already collect and are already checked
against ground truth — `cpu.profile` against its own symbols, `sched.runqlat`
against a known number of blocking round trips, `sched.offcpu` against a thread
that sleeps a known 300 ms — so nothing here is new mechanism. What is new is
the arithmetic that adds them up, and the honesty about what the sum is missing.

`examples/sched-half-blocked.yml` is the workload: a program half on-cpu and
half blocked, written by the scenario wave for exactly this.

**Three things must not be quietly wrong.**

- **The three states must account for the wall clock, or say what is
  unaccounted.** A decomposition whose parts do not sum is a decomposition that
  is hiding a fourth state. Print the residue.
- **A sampled profile and a traced off-cpu series are not the same kind of
  number.** One is a sample count scaled by a period, the other is a measured
  duration. Where you put them in one table, the table says which is which.
- **Every percentile you take is now marked if the run was lossy.** That is
  stream F's `Shortfall`, and it is on `SeriesHist` and `SeriesSummary` already.
  Your document should show a lossy run's decomposition beside a clean one, so a
  reader sees what the mark changes.

---

## Stream P3 — the I/O path

> **Mission.** What one read cost, from the syscall to the platter. The join the
> sixth wave unblocked: `blk:rq@open_ts` with `scoped_eq`, `io.queue` into
> `io.latency`, at 99.9 % on a five-second run.

`docs/ROADMAP.md` phase 4 is the decomposition this is half of, and its boundaries 5,
6 and 7 are the ones you can reach today: FS to block submission, submission to
completion, completion to the wakeup that released the reader. **Boundaries at
the syscall edge need `fentry`, which this wave does not have** — `io.syscall`,
`io.fs` and `io.fsync` have no collector and are behind stream S's rewrite of
`attach.rs` plus a wave after this one. So your crossing starts at the block
layer and says, in its own output, where it starts. A decomposition that omits
its first segment without saying so is the failure this project exists to
prevent.

**The scoped key is the thing to show.** Drive enough I/O that a request pointer
is recycled inside one run — the merge commit caught the same pointer under two
open timestamps four milliseconds apart — and show that no completion was
attributed to a request that had already finished. That check is the whole
argument for the key being scoped rather than a pointer, and it belongs in a
test and not only in a document.

Evidence levels, on every line of the decomposition: boundary 6 is `IDENTITY`,
boundary 7 is `KEY`, boundary 8 is `WAKEUP`. A reader has to be able to tell a
propagated identity from two things happening at the same time.

---

## Stream P4 — crossed families, and the machine as a confounder

> **Mission.** The question that invalidates other people's measurements:
> **does the machine's own state explain what you just measured?**
> `power.freq` and `power.throttle` against `cpu.instructions`;
> `mem.pressure` against `io.bytes`.

This is the one crossing with no single headline number, and its output is a
verdict rather than a decomposition: *this run was thermally throttled for 1.2 s
of its 5 s, and the instruction rate over that interval is not comparable to the
rest*. Phase 8's harness invalidates a throttled run outright; you are the
measurement that tells it to.

**`mem.pressure` against `io.bytes` is the second half and the more interesting
one**: under memory pressure the kernel reclaims, reclaim writes back, and the
I/O you measure is the machine's and not the workload's. `mem.reclaim` collects
since the sixth wave, so for the first time that claim can be shown rather than
asserted.

**Trap.** Both halves are correlations over time on a shared machine, which is
`TEMPORAL` evidence — the weakest rung, and the one the ladder exists to keep
honest. Do not dress it as anything else. A crossing that says *these two moved
together* with `TEMPORAL` beside it is worth more than one that implies a cause.

---

## Stream S — the shared layer nobody owns

> **Mission.** The twenty requests the sixth wave left under
> `## Needs from another stream`, landed once. You own the shared eBPF layer and
> `catalog/*.yaml`. Your handoff is `docs/families/shared-layer.md`, new.

Read every `## Needs from another stream` section in `docs/open-findings.md`
first and group them, because three are asked for twice by two families each and
the point of this stream is that they are written once:

- **`Btf::member_offset` that descends anonymous members** — asked by `io` and
  by `net`. `folio.mapping` lives inside an anonymous union, and two sources
  (`io.pagecache`, `io.readahead`) wait on nothing else.
- **The values and names of an enum, by name** — asked by `io` and by `net`.
- **The helper table**: `bpf_get_current_comm`, `bpf_get_current_cgroup_id`
  (80), a bitwise `and`, and the three program fragments now written three
  times.
- **A cgroup target for `sched`** — six entries declare `cgroup: bpf_only` and
  six collectors refuse it. The eBPF half is one helper; the other half is
  `name_to_handle_at` on the cgroup directory, and belongs wherever
  `Target::Cgroup` is already resolved.
- **The catalog corrections**, each with the family that filed it: `sched.irq`
  requires only the hard half of what it promises, `mem.reclaim` requires half
  of the source it names, `mem.compaction` requires a beginning and no ending,
  two `mem` entries declare a key nothing can carry, `direction` is not a filter
  field, and three key declarations have no emitter.

**The rule that makes this stream safe: a request is landed with the collector
that wanted it, or it is not landed.** A helper added with no caller is a
signature invented from a description — write the caller, or leave the request
in the file with a note saying why it was not taken. Where you change a catalog
entry's requirements, a source that was reported available and would have failed
at attach is finding 15 all over again: re-probe and check what moved.

**Do not start `fentry` here.** It is the next wave's, it is real mechanism, and
this stream's value is that it is boring and finite.

---

## Stream E — `rate`, `topk` and `stack` are short in the same way

> **Mission.** Finding 37 was scoped to `hist`, `summary` and percentiles because
> three documents had agreed that scope. The argument for widening it is in
> `docs/open-findings.md`, written by stream F on its way out.

A run that dropped events understates its `rate`, its `topk` totals and its
`stack` weights by exactly the same one-sided amount and for the same reason: the
records a ring drops are the ones from the busiest moment. `Shortfall::of` is
public and takes `(&Losses, records, per_record)`, so there is no new derivation
to do. **What there is, is a decision about how the mark reads next to what
those emits already say about themselves**: `Output::Stack` carries
`unattributed` and `Output::TopK` carries `Rest`, and a mark bolted beside
either is a second statement about missing data in different words. Make them
one statement or keep them visibly separate, and write down which you chose.

**Your second file is `src/encode.rs`, not `src/render.rs`.** The sixth wave's
brief named the wrong one: `render.rs` renders requests, listings and refusals;
`encode.rs` renders results, and `hist_text` is in it. Stream F declared the
overstep rather than hiding it, which is why you have the right file list.

`format.rs` declares what each format can carry and a test crosses that claim
against reality, because a merge once reverted an encoder and left the claim
behind. Every format that can carry a `topk` or a `stack` carries the mark too,
or says it cannot.

**And the negative matters as much as the positive: a clean run must gain
nothing**, or the mark becomes noise and readers learn to ignore it.

---

## Rules

The three that have held for seven waves:

1. **The unit of ownership is a file set nobody else writes.** Here it is a
   file-name prefix for the four crossings, because a crossing belongs to no
   family, and a named file list for S and E.
2. **The handoff is a file only that stream can write**, and it is the
   deliverable rather than a log about one:
   `docs/scenarios/cross-<name>.md` for P1 to P4,
   `docs/families/shared-layer.md` for S, `docs/families/emit.md` for E.
3. **A crossing is landed when it says what evidence it has.** `IDENTITY` >
   `KEY` > `WAKEUP` > `TEMPORAL`, on every joined result. A correlation backed by
   a propagated identity and one backed by two things happening at the same time
   are both correlations, and only one of them is evidence.

A stream edits only its own paths. Needing something from another stream's files
is not a reason to edit them: write it under `## Needs from another stream` in
`docs/open-findings.md`, under a heading naming your stream, with the exact
signature wanted, and carry on.

**Build with `make`, not `cargo build`.** The preload shim is a workspace member
nothing depends on, and `CAP_BPF` and `CAP_PERFMON` are file capabilities every
relink drops. Both failures are silent: the tool reports fewer sources as
available, which reads like the machine changed rather than the binary. Half of
what every crossing here joins is an eBPF source, so a green suite on an
unprivileged binary says nothing about any of it. `make check` clean before you
hand over.

One branch per stream, made by `scripts/spawn.sh`. Commit there; the coordinator
merges.

**A last word on tests.** Three of the last five sessions on this project lost
time to a test that asserted what the machine was doing rather than what the
tool did, and this wave is more exposed to it than any before: a crossing is two
sources on a shared machine at the same time, and the second-order effects the
scenario wave hit — a shared GPU, a namespace given 250 ms, a parallel suite
sharing the caches — are the normal case here rather than the edge. Assert the
*separation* rather than a calibrated threshold, carry the reference's own
resolution as slack, and where the property cannot be created, skip loudly with
the numbers that show why. A scenario that states its limit is worth more than
one that hides it.
