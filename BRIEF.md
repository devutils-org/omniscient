# Brief — scenarios: making the tool answer questions, and checking it told the truth

Eight work streams for phase one, four for phase two. This is the fourth
fan-out, and it is the first that adds **no collector at all**.

The three previous waves built the thing. Ninety-nine of a hundred sources
resolve, fifty-nine collect, all seven emits and all four join match kinds
exist, nine output formats, two stack mechanisms, a measured cost and an
enforced budget. What none of that establishes is the only thing that matters
to someone holding the tool: **does it answer a real question, and is the
answer right?**

So this wave writes scenarios. A scenario is not a demo.

---

## What a scenario is

Three parts, and it is not one without all three:

1. **A workload with a property the scenario chose.** Not observed — chosen.
   Allocate forty megabytes and touch every page. Retire a hundred million
   instructions. Sleep three hundred milliseconds. Write sixty-four megabytes
   and `fsync`. Contend a mutex ten thousand times. The number is in the
   workload's source, which is what makes it ground truth.

2. **A session that measures it**, as `examples/<family>-<name>.yml` — the same
   object a command line builds, written down, so a reader can run it and a
   test can run it identically.

3. **A check.** A test in `tests/scenario_<family>.rs` that runs the session and
   asserts the answer against the property in (1) — or, where the answer cannot
   be checked, an explicit statement in `docs/scenarios/<family>.md` of what it
   cannot verify and why. "It ran and printed something" is a demo. The
   repository has demos; this wave is not adding more.

A scenario that measures what the tool computed proves nothing. That rule has
been the bar for three waves and it is the whole of this one.

---

## The four scenario classes, per family

Every family covers all four. They are different questions and each catches a
different kind of lie.

### A. The capability sweep — *does every axis the catalog declares work?*

For each source with a collector: every **mode** it declares, every **emit**,
and at least one **format** beyond `text`. The catalog is data and the CLI is a
projection of it, so a combination the catalog offers and the build cannot
serve is a promise broken in the one place nobody looks.

What this catches: an emit that renders wrong on a source whose events are
shaped unusually, a mode that resolves and then refuses at attach time, a
format that claims an emit it cannot carry.

### B. The backend comparison — *do two roads to the same number agree?*

`--backend force:<id>` exists for exactly this and is used by nothing. Where a
chain has more than one **implemented** backend, measure the same workload
through each and state the relationship: equal, or different by an amount the
degradation explains.

Today that is eight sources: `mem.rss`, `.pss`, `.uss`, `.thp`, `.swap`,
`.pressure`, `mem.cache_llc` and `power.energy`. If your family has none, say so
in your document and spend the effort on class D instead — do not invent a
comparison that is not there.

Where a chain has one implemented backend and others declared, the comparison
is with the **refusal**: forcing the unimplemented one must exit 3 and name what
is missing, not exit 1 like a failure.

What this catches: the case the catalog is built for. `fs:proc:smaps_rollup` and
`fs:proc:statm` are the same number by two routes, and a run where they disagree
is either a bug or a caveat nobody wrote down.

### C. The loss path — *does the tool say what it did not see?*

Drive the source hard enough to lose something, and check the loss is counted
and reaches the output. A ring that overflows, a map that fills, a counter the
kernel throttles, a poll that runs late, a target that exits mid-run.

This is the class that justifies the project. A collector that loses three
percent and says nothing produces wrong numbers that look right, and by the
time anyone notices the run is gone. Every family has at least one way to lose
data; find yours and make it happen on purpose.

### D. The refusal — *is the reason the right reason?*

Three different things the tool says no to, and they must not be confused:

| It says | It means | Exit |
|---|---|---|
| a rejection | the request cannot mean anything | 2 |
| no viable backend | this machine cannot serve it | 1 |
| no collector | this build does not implement it | 3 |

For each, check the message names the thing that is actually missing. A refusal
that sends a reader to the wrong sysctl, the wrong capability or the wrong flag
costs more than no message at all — and `docs/open-findings.md` already records
two that did.

---

## The streams

| | Stream | Sources it covers | Owns |
|---|---|---|---|
| **A** | `cpu` scenarios | 11 | `examples/cpu-*.yml`, `tests/scenario_cpu.rs`, `docs/scenarios/cpu.md` |
| **B** | `mem` scenarios | 13 | `examples/mem-*.yml`, `tests/scenario_mem.rs`, `docs/scenarios/mem.md` |
| **C** | `io` scenarios | 5 | `examples/io-*.yml`, `tests/scenario_io.rs`, `docs/scenarios/io.md` |
| **D** | `net` scenarios | 7 | `examples/net-*.yml`, `tests/scenario_net.rs`, `docs/scenarios/net.md` |
| **E** | `sched` scenarios | 5 | `examples/sched-*.yml`, `tests/scenario_sched.rs`, `docs/scenarios/sched.md` |
| **F** | `power` scenarios | 5 | `examples/power-*.yml`, `tests/scenario_power.rs`, `docs/scenarios/power.md` |
| **G** | `gpu` scenarios | 5 | `examples/gpu-*.yml`, `tests/scenario_gpu.rs`, `docs/scenarios/gpu.md` |
| **H** | `proc` and `runtime` scenarios | 4 | `examples/proc-*.yml`, `examples/runtime-*.yml`, `tests/scenario_proc.rs`, `tests/scenario_runtime.rs`, `docs/scenarios/proc.md`, `docs/scenarios/runtime.md` |

**The file-name prefix is the lock.** Every stream's files are named after its
family, so the eight sets cannot overlap and no merge can conflict. That is
deliberate: the last wave had six streams append to one findings file and
produced six conflicts on it.

Each stream **inherits the examples already named for its family**, and may
extend, rewrite or replace them — they were written to be read, not to be
checked, and a scenario is the stronger thing. Five files carry no family
prefix because they belong to no family, and they are the coordinator's:

| File | Why it is not yours |
|---|---|
| `allocation-cost.yml` | a join across two sources; phase two's P1 starts from it |
| `web-latency.yml` | five families at once; phase two's shape |
| `saturation.toml` | a sweep over every resource, and the only TOML session |
| `filters-and-formats.yml` | about the axes rather than about a family |
| `examples/README.md` | the index, regenerated from what lands |

**`examples/README.md` and `scripts/demo.sh` belong to the coordinator**, who
regenerates both from what lands. Do not edit them. Your
`docs/scenarios/<family>.md` is where you write for a reader, and it is also
your handoff note — the coordinator reads that, not your diff.

**No stream edits `src/`.** If a scenario finds a defect, that is the scenario
doing its job: write it in `docs/scenarios/<family>.md` under a
`## Defects found` heading, with the session that shows it and the output it
produced, and keep going. A scenario suite that quietly worked around three
bugs has measured nothing. If the defect makes a whole class unreachable for
your family, say which class and why.

The one exception: a **workload** is code, and yours to write. Put it in
`tests/scenario_<family>.rs` or beside it — a Rust helper, a shell one-liner in
the session's `launch:`, a small Python program. Prefer a workload whose
property is obvious from reading it over one that is clever.

---

## Phase two — the crossings

Not dispatched yet. It is written here because phase one should leave it
possible, and because a scenario that cannot later be joined to another family
is a scenario that stopped early.

Four streams, each a **question a single family cannot answer**:

| | Stream | The question | The join |
|---|---|---|---|
| **P1** | Memory, end to end | Which allocation cost memory, and did it stay? | `heap.alloc → mem.faults` (interval, exists), then `→ mem.rss` and `heap.retained` |
| **P2** | Compute, end to end | Where did the time go — on cpu, or waiting? | `cpu.profile` beside `sched.offcpu` by task, and `sched.wakeup → sched.offcpu` (wakeup) for what released it |
| **P3** | The I/O path | What did one read actually cost, from syscall to platter? | `io.*` by `blk:rq@open_ts` (scoped_eq) once the `io` eBPF backends land; today, `sched.offcpu` state `uninterruptible` against `io.pressure` |
| **P4** | Crossed families | Does the machine's own state explain the measurement? | `power.freq` and `power.throttle` against `cpu.instructions`; `mem.pressure` against `io.bytes`; the confounders that make two runs incomparable |

What phase one must leave behind for these:

- **Every session that could carry a key, carries it.** A scenario measuring
  `mem.faults` without `keys: [task, mem]` produces events that cannot be joined
  to anything, and re-running it later is the same work twice.
- **A workload that phase two can reuse.** If your family's workload is a
  program that allocates, or blocks, or writes — say so in your document under
  a `## Workloads` heading, with what property it guarantees. P2 needs a program
  that is half on-cpu and half blocked; someone in phase one will have written
  one.
- **The name of what your family contributes to a crossing.** One line: what
  `cpu` knows that `mem` does not, and which key carries it.

---

## Rules

Read `STATE.md` for what the tool can do, `CATALOG.md` for what the axes mean,
`docs/contributing.md` for what ground truth means here, and `CLAUDE.md` for
the invariants.

`make check` must be clean when you finish. A scenario test that is slow is
worse than one that is quick and narrow: the suite runs on every change, and
eight hundred tests already run in under three seconds. Keep a scenario under a
second where you can, and say in your document which ones are not and why.

**A test that skips is not a test that passes.** Where this machine cannot serve
a scenario — no GPU, no capabilities, no PTP clock — the skip must name what is
missing, and the scenario must still be in `examples/` and in your document, so
that a machine that *can* serve it runs it. A machine's limit is a reason to
stop quietly; a request that cannot mean anything is not.

**Privileges.** The eBPF and per-cpu perf backends need `CAP_BPF` and
`CAP_PERFMON`, which every relink drops — build with `make`, not `cargo build`.
`net.capture` additionally needs `CAP_NET_RAW`, which the build does not grant;
if your family needs it, say so in your document rather than changing the
Makefile.

One branch per stream, `feat/scenarios-<family>`, in its own worktree, made by
`scripts/spawn.sh <family>-scenarios`. Commit there; the coordinator merges.
