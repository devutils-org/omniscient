# Brief — the sixty-three, divided by what is wrong rather than by where

Six work streams. This is the fifth fan-out, and it is the first divided by
**defect class** instead of by family or by layer, because that is the shape the
work now has.

The fourth wave wrote 159 scenarios and found 63 defects, none of them worked
around. They are in `docs/scenarios/<family>.md`, each with the session that
shows it and the output it produced. Read them: this brief groups them, it does
not restate them.

Grouping them by family would put twelve copies of one bug in twelve places.
Nine families independently found that a refusal's exit code does not match its
message; seven found that a loss reason carrying a measurement is a new reason
every time, so losses never aggregate. Those are two fixes, not sixteen.

---

## The streams

| | Stream | Owns | Defects |
|---|---|---|---|
| **A** | Exit codes and the runner | `src/main.rs`, `src/cli.rs`, `src/collect/mod.rs`, `src/error.rs` | ~24 |
| **B** | What the catalog claims | `catalog/*.yaml`, `src/validate.rs` | ~11 |
| **C** | Emit, filter, render | `src/emit.rs`, `src/filter.rs`, `src/encode.rs`, `src/render.rs` | ~7 |
| **D** | Stacks, and the two families that produce them | `src/stack.rs`, `src/collect/cpu/**`, `src/collect/sched/**` | ~9 |
| **E** | `mem`, `proc`, `power` readings | `src/collect/{mem,proc,power}/**` | ~8 |
| **F** | `net`, `io`, `gpu`, `runtime` readings | `src/collect/{net,io,gpu,runtime}/**` | ~10 |

**A is the one that touches everything.** It is also the one worth doing first
if fewer hands are available: twelve of the sixty-three are one wrong `match`
arm, and every family's scenario document opens by saying so.

---

## The seam, named in advance

**Scenario tests assert the defects.** Fixing a defect means the scenario that
recorded it now fails, and the scenario has to be updated to assert the fixed
behaviour — that is the fix being complete, not an afterthought. So:

> A stream may edit any assertion in `tests/scenario_*.rs` **that names a defect
> it fixed**, and must say which in its log. It may not touch anything else in
> those files.

Stream A will touch several of them, because exit codes are asserted in every
family. That is expected; the edits are one line each and the coordinator
resolves what collides.

**Each stream also updates the `## Defects found` section of every
`docs/scenarios/<family>.md` whose defects it fixed**, marking each fixed one
and leaving the reproduction in place. A defect that is gone and still
documented as present is the same kind of lie this project is about.

---

## Stream A — exit codes and the runner

> **Mission.** Two cross-cutting bugs that nine families found independently,
> plus the session runner's own. You own `src/main.rs`, `src/cli.rs`,
> `src/collect/mod.rs` and `src/error.rs`.

**The exit codes.** There are four answers and three codes, and two of the four
are wrong. Measured:

| It says | It means | Returns | Should |
|---|---|---|---|
| `cannot be measured per …` | the request cannot mean anything | 2 | 2 |
| `has no viable backend` | this machine cannot serve it | 2 | **1** |
| `no collector is written for … yet` | this build has not written it | 3 | 3 |
| `cannot start collecting` | a collector was chosen and refused at attach | 1 | **3**, where the reason is a mode or shape the collector does not implement |

`GUIDE.md` has the table a reader sees. The fourth row is the subtler one: a
collector refusing at attach because the *machine* cannot serve it is a genuine
1, and one refusing because the *build* does not implement that mode is a 3.
`CollectError::Start` cannot tell them apart today, which is the change.

Defects: cpu 4 and 6, gpu 4, io 1 and 3, mem D5, net 7 and 8, power 2, proc 4,
sched 6 and 9.

**The loss reasons.** `Losses::note` keys by the reason string, and several
collectors format a measurement into it — "a tick ran 41.3 ms late". Every
occurrence is then a distinct reason with a count of one, so a run that ran late
two hundred times prints two hundred lines and aggregates none of them. The
reason has to be the *kind*, with the measurement carried beside it.

Defects: cpu 8, gpu 5, io 5, net 2, proc 6.

**The runner.** A session drives every collector at the pace of its fastest
request, so a `snapshot` beside a `poll@10ms` is polled and a `poll@1s` beside
it runs at 10 ms. Request order therefore decides another request's sampling
rate, which is the one thing a declarative session must not do.

Defects: io 6, net 4, power 3 and 4, proc 3, mem D7, cpu 5, sched 7 and 8.

**Traps.** `--duration` beside `-f` is accepted and discarded (cpu 5), and a
JSON run shares stdout with the program it launched (cpu 10) — both yours.
Changing an exit code changes what every scenario asserts; see the seam above.

---

## Stream B — what the catalog claims

> **Mission.** Eleven places where an entry promises something no mode, no
> backend or no collector of that entry can produce. You own `catalog/*.yaml`
> and `src/validate.rs`.

The catalog is data and everything else is a projection of it, so a claim it
makes that cannot be served is a lie in the one place nobody looks. Three
shapes, and they are fixed differently:

- **An emit no declared mode can produce.** `mem` has two, `net.bytes` has one.
  Either the emit or the mode is missing from the entry; the scenarios say
  which. (mem D3, net 1)
- **A key or filter field no backend carries.** `proc.fd` declares the `file`
  key and no event has one; `io` accepts a key its backend cannot produce
  silently. The per-backend `keys:` and `filter_fields:` model exists — use it.
  (proc 5, io 8)
- **A hint that offers what the source refuses.** A mode's rejection suggests an
  emit that source does not accept, which sends a reader in a circle. (io 2)

The rest: gpu 3 (`rate` of a ratio is a quantity of nothing), net 3 and 5,
sched 5, runtime 1, io 7.

**Trap.** The loader already refuses several kinds of drift between an entry and
its backends. Where a fix is "the loader should have caught this", add the check
rather than only the data — that is what stops the next one.

---

## Stream C — emit, filter, render

> **Mission.** Seven defects in what a number looks like by the time a reader
> sees it. You own `src/emit.rs`, `src/filter.rs`, `src/encode.rs` and
> `src/render.rs`.

- **`sched.offcpu --emit hist` renders a distribution that is not one** (sched
  1). The likely cause is the one `emit::WEIGHT_ATTR` was added for, in a source
  that does not yet carry it — check before assuming.
- **`=~` is a substring test and nothing says so** (runtime 2). A reader writing
  `comm =~ 'python'` gets `python3` and does not know whether that was anchored.
- **`daddr ==` can never match** (net 6).
- **A refusal sentence pushed into a slot meant for one word** (mem D4).
- **`topk` ranks processes, not the methods that were compiled** (runtime 1) —
  shares a boundary with stream B; the scenarios say which side it falls on.

**Trap.** `format.rs` declares what each format can carry and whether its
encoder exists, and there is a test crossing the claim with reality because a
merge once reverted an encoder and left the claim behind. Keep it true.

---

## Stream D — stacks, and the two families that produce them

> **Mission.** Stacks are the most expensive thing this tool does and the
> measurements around them are the least checked. You own `src/stack.rs`,
> `src/collect/cpu/**` and `src/collect/sched/**`.

- **A blocking stack of a *launched* program comes back with no names** (sched
  2). The symbolizer snapshots `/proc/<pid>/maps` when built, and for a launched
  program `start` is too early and the first drain is often too late. This is
  the third time this trap has been hit in this project; `STATE.md` records the
  other two.
- **`--emit stack` costs about 0.65 s per process, and machine-wide never
  finishes** (sched 3). Symbolizing every process of a machine is the cost, and
  a run that cannot finish is worse than one that refuses.
- **A profile costs a second of wall clock before it samples anything** (cpu 7).
- **A profile follows one task, finds nothing, and reports no loss** (cpu 1) —
  the threads it did not follow are a degradation, not silence.
- **`cpu.util` loses nine tenths of its readings to the kernel's tick** (cpu 3),
  and **a throttled period is counted as a dropped sample** (cpu 2) although
  `Losses` now has a `throttled` field of its own.
- **A crowded pmu is reported as a target that never ran** (cpu 9).
- **`sched.migrations --mode count` is `--mode trace` under another name**
  (sched 4).
- **`tests/family_sched.rs` fails about one run in five** (sched 10).

**Trap.** The eBPF and per-cpu perf backends need `CAP_BPF` and `CAP_PERFMON`,
which every relink drops. Build with `make`. A green suite on an unprivileged
binary says nothing about half of what you own.

---

## Stream E — `mem`, `proc`, `power` readings

> **Mission.** Eight places where a collector reports a number that is not what
> it says. You own `src/collect/{mem,proc,power}/**`.

- **`mem.rss` through `fs:proc:statm` invents a zero for a process that has
  gone** (mem D1). Absent is not zero, and this is the shape of that rule.
- **The shim stops counting its drops as soon as it is dropping** (mem D2) —
  the one loss counter that fails exactly when it is needed.
- **The two `mem.thp` backends disagree about resident huge pages, and nothing
  says so** (mem D6). Either they are equivalent and one is wrong, or they are
  not and the entry must say what is degraded.
- **The first reading of a launched target describes this tool** (proc 1) and
  **the last reports a zombie as holding nothing** (proc 2). The first is the
  same trap as sched 2 above; coordinate with stream D rather than solving it
  twice.
- **`power.energy -m count` is a series plus a zero, and the two backends
  disagree about what `count` means** (power 1).
- **A unit test that fails under load** (power 5).

---

## Stream F — `net`, `io`, `gpu`, `runtime` readings

> **Mission.** Ten defects across four families, sharing no mechanism. You own
> `src/collect/{net,io,gpu,runtime}/**`.

- **A dropped interval, noted and not counted, in a run reporting `no loss`**
  (io 4), and **the loss headline names three of the five things that are
  counted** (io 9). The second is stream A's type and your headline; say in
  `docs/open-findings.md` what you need if the split is wrong.
- **`io.bytes` measures the read/write split and no emit can show it** (io 7).
- **`snapshot` on a counter source can only ever answer zero** (net 3).
- **Every aggregating emit of `net.iface` answers about errors alone** (net 5).
- **A `count`-mode run over-reports its own window by up to a second** (gpu 2),
  **the one time error this family records reaches no output it can produce**
  (gpu 1), and **a target that exited but was not reaped is followed to the end
  of the run** (gpu 6).

**Trap.** `gpu` needs an NVIDIA device and `net.capture` needs `CAP_NET_RAW`,
which the build does not grant. A fix you cannot exercise here is a fix you
cannot claim; say so rather than asserting it works.

---

## Rules

Read `STATE.md` for where the project is, `GUIDE.md` for how to run what you are
fixing, `docs/scenarios/<family>.md` for the defects themselves, and `CLAUDE.md`
for the invariants.

A stream edits only its own paths, plus the scenario assertions and defect
entries named in the seam above. Needing something from another stream's files
is not a reason to edit them: write it under `## Needs from another stream` in
`docs/open-findings.md` with the exact signature wanted, and carry on.

**A defect is fixed when the scenario that found it asserts the new behaviour
and would fail against the old.** A fix with no test is a claim.

`make check` clean, build with `make`. One branch per stream,
`feat/fix-<stream>`, made by `scripts/spawn.sh fix-<stream>`. Commit there; the
coordinator merges. Your log is `docs/fixes/<stream>.md`: what you fixed, what
you found to be something else, and what you left and why.
