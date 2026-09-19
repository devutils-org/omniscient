# `runtime` — what is worth measuring, and how you know the answer is right

One source of this family collects: `runtime.jit`, over `fs:jit-map`. Three do
not, and the fourth has a second backend that does not either — all four wait on
`bpf:usdt`.

This is the layer above the kernel, and without it a GC pause or a JIT
deoptimization gets attributed to the wrong resource. It is also not a closed
family: `runtime.custom` is the extension point where a user declares a probe of
their own.

Sessions are in `examples/runtime-*.yml`, checks in `tests/scenario_runtime.rs`.

---

## Ground truth in this family comes in two shapes

Neither is sufficient alone, so both are used.

**Against a written map.** `/tmp/perf-<pid>.map` is a plain text file by
convention, so a shell workload can write its own: the number of entries, their
sizes, their names, and the exact moment the file is truncated or deleted are
all *chosen* rather than observed. That gives exact counts, and it is the only
way to reach the loss paths on purpose. What it does not prove is that a real
runtime's output is read correctly.

**Against a real V8.** The workload compiles a function whose name no plausible
constant could produce, and node publishes what it compiled. Here the count is
*not* ground truth — how many times V8 decides to compile a function is V8's
business, and asserting a number would be asserting a V8 version. The **name**
is: an event naming `omniscientGroundTruth` came from a real JIT's real map, and
nothing else in the system could have produced that string.

Where a scenario needs a real runtime and this machine has none, the skip names
what is missing and says which half of the coverage is lost — the written-map
scenarios still run and cover everything except whether a real runtime's format
is read.

---

## Scenarios

| Question | Session | What makes the answer checkable |
|---|---|---|
| What did the runtime compile, and when? | `runtime-jit.yml` | a real V8 compiles `fib`; the name is in the program's source and in no constant this tool could invent |
| Does every axis the catalog declares work? | `runtime-jit-sweep.yml` | the workload writes exactly 8 entries of 0x10 bytes with chosen names, and filters keep 1 and 8 of them |
| Does this source say what it did not see? | `runtime-jit-loss.yml` | 6 good lines, 2 unparseable, one truncation, one deletion — four counters, four chosen numbers |
| Do two roads to the same number agree? | `runtime-backends.yml` | one road measures; the other is a refusal, and must exit 3 |
| Is the reason the right reason? | `runtime-not-built.yml` | valid, resolvable, unwritten: exit 3 naming `bpf:usdt` |

### A — the capability sweep

`runtime.jit` declares one mode (`trace`), five emits (`raw`, `rate`, `topk`,
`timeline`, `summary`) and four filter fields (`comm`, `method`, `code_kind`,
`bytes`). All of them run. Every structured format carries every emit; the stack
formats refuse by name, because this source has no stacks and an empty profile
would read as a program that compiled nothing.

One emit runs and cannot answer the question it looks like it answers — defect 1.

The filter half matters more here than in most families, because most of a real
node map is V8's own precompiled builtins arriving through the same file. They
are not compilations the program caused, and the catalog caveat tells a reader
to filter on `code_kind` to separate them. `runtime-jit.yml` does exactly that
and the run says what it removed: `filter code_kind == 'js' kept 119 of 2457
events, evaluated after reading`. A scenario that did not filter would be
measuring node's startup, not the program.

That attribute is called `code_kind` and not `kind` for a reason recorded in an
earlier wave: the filter grammar reads `kind` as the event's source id, so an
attribute named `kind` would be declared, validated, reported as applied, and
match nothing. The sweep checks that the field selects rather than merely being
accepted, because that is the failure this family has already had once. It has a
second one now — defect 2.

### B — the backend comparison

`runtime.jit` has two backends, `bpf:usdt` then `fs:jit-map`, and one of them is
written. So the comparison this family can make is not between two numbers; it
is between a measurement and a refusal, which is what `BRIEF.md` prescribes for
exactly this case.

`--backend force:fs:jit-map` measures, and says twice that this is not a
measurement to keep: a warning before the run and a degradation in the output.
It also carries the declared degradation, which is the whole reason it is second
in the chain — the map names the code that was compiled and when it was first
*seen*; it does not time the compilation, and it carries no deoptimizations. A
source that timed a compilation from this file would be inventing the number.

`--backend force:bpf:usdt` exits 3 and names the backend. Not 1, which would
read as a run that failed. The two cannot share a session, because a session
stops at the first source it cannot serve and nothing would print — so
`runtime-backends.yml` holds the measurement and `runtime-not-built.yml` holds
the refusal.

### C — the loss path

Four ways this source loses data, and `runtime-jit-loss.yml` reaches all four in
one run because the map is a file the workload can abuse on purpose:

| Counter | What it means | How the workload causes it |
|---|---|---|
| `undecoded` | a line arrived and could not be read | 2 lines in no format this build knows |
| `dropped` | the file was replaced | truncated, so it is shorter than what was already consumed |
| `failed_reads` | the map went away mid-run | deleted while the program still runs |
| — | *not* a loss | a map not yet written at the first poll |

They are separate counters because they are different failures, and a collector
that folded them into one number would send a reader looking in the wrong place.
The fourth line matters most: a map that has never been read *yet* is not a gap,
because a runtime creates its map as it starts and looking a few milliseconds
early costs nothing — but a map that is never read *at all* is this run watching
a program and seeing nothing, which is not the same as a program that compiled
nothing. That is the one statement this source exists to avoid making, and the
reason it gives names what a reader can actually do about it: start the runtime
differently, with `--perf-basic-prof` or a JVM agent, rather than run this tool
differently.

A fifth thing is reported and is not a loss: the timestamp uncertainty. The file
records that code exists, never when it was created, so a line is known to have
appeared between two reads and no more precisely and carries that gap as
`ts_err_ns` — and a line already there at the first read is older than the run by
an amount nothing records, and carries `u32::MAX`, the largest error the event
model can express. The scenario writes three lines, waits, and writes two more,
so which line falls in which class is chosen rather than observed.

### D — the refusal

`runtime-not-built.yml` covers the build limit: one written source pinned to its
unwritten backend, and three sources with no collector at all. All exit 3, all
name `bpf:usdt`. The session validates cleanly first, which is what makes the
answer "not written yet" rather than "you asked for something impossible".

The rejection side is checked too, on the one source that offers it naturally:
`runtime.alloc` does not declare `raw` — it is an allocation profile and its
emits are the ones that aggregate — so asking it for `raw` exits 2, not 3, and
names the emits it does declare.

---

## Workloads

**The written-map script** (`runtime-jit-sweep.yml`, `runtime-jit-loss.yml`,
`runtime-backends.yml`). `sh`. Writes `/tmp/perf-$$.map` itself, one line per
entry in the `<hex address> <hex size> <symbol>` format, sleeping between steps
so the collector's 10 ms read interval can see each state before the next.
Guarantees: an exact number of entries with chosen names and sizes, and a file
whose truncation and deletion happen at moments the workload picks. Removes its
own map. This is the workload to copy for anything that needs a JIT-shaped event
stream without a JIT.

**The V8 program** (`runtime-jit.yml`). `node --perf-basic-prof
--logfile=/tmp/omn-v8-%p.log`. Defines a recursive `fib` and calls it enough
times that V8 stops interpreting and compiles it. Guarantees: a named function
in a real map, and a realistic ratio of builtins to program code — about 2450
builtins to 120 JS entries — which is what makes the `code_kind` filter worth
testing against something real. The `--logfile` is not optional: without it V8
writes `isolate-*.log` into the working directory, which for a test is the
repository.

Neither is a P2 workload. P2 needs a program that is half on-cpu and half
blocked, and both of these are almost entirely one or the other.

---

## What this family contributes to a crossing

`runtime` knows **what the layer above the kernel did**, which by construction no
other family can see. A JIT compilation exists in no file on disk and no kernel
counter; the runtime is the only thing that can report it.

The key is `task`, and `runtime-jit.yml` sets it. The crossing it is for is the
one this source was written for: a `cpu.profile` sample taken inside generated
code points at an address that belongs to nothing, and `runtime.jit` is the
table that names it. The join is by address rather than by key, which no match
kind currently expresses — `cpu.profile` already symbolizes through the target's
own maps, and whether that path reads the JIT map is a question for stream A's
document, not this one.

`runtime.alloc` would contribute the other half of P1: the runtime's own
allocation — an object, a boxed value, a generation — which is *not* the C
allocator beneath it. For a managed runtime the two are unrelated, since V8 maps
its heap directly and never calls malloc for it, so `mem.heap.alloc` and
`runtime.alloc` measure different things and neither substitutes for the other.
It has no collector.

---

## What cannot be verified here

**Three of four sources, entirely.** `runtime.gc`, `runtime.alloc` and
`runtime.custom` have no collector. Their refusal is checked; nothing about
their measurements can be.

**`runtime.gc`'s central promise.** Its caveat says: *"A runtime built without
its probes reports no probe, which is not the same as a runtime that did not
pause. The refusal names the missing probe."* That is the most important
sentence in this family — absence is the normal case, since the python3 of most
distributions is built without `--with-dtrace` and a Java runtime needs
`+ExtendedDTraceProbes` — and **nothing can check it today**. What the tool
actually says is `no collector is written for runtime.gc on backend bpf:usdt
yet`, which names the backend and not the probe. That is the correct answer for
this build, and it means the caveat describes a refusal no build can currently
produce. Worth re-reading the day `bpf:usdt` lands, because a caveat that
promises a message nobody has written is the kind of thing that stays unwritten.

**Whether `usdt` is present on this machine.** `omn list runtime` reports `?` for
the three unimplemented sources: no probe is written for the `usdt` requirement,
so the resolver marks the backend `UNVERIFIED` and says it was assumed rather
than checked. That is honest and loud, and it means this machine's answer to
"could it collect these" is unknown, not yes.

**The number of compilations a real runtime performs.** Asserted nowhere, on
purpose. It is a property of the V8 version, not of the program, and a test that
pinned it would fail on a node upgrade for no reason worth acting on.

**How long a compilation took.** Not a limit of the scenarios but of the
backend, and it is the reason `bpf:usdt` is first in the chain. Stated in the
degradation that travels with every run.

**Anything about deoptimizations.** Same reason: the map carries none.

---

## Defects found

Two. Both are in the shared layer rather than in this family and were found
through it; they are the coordinator's to route into `docs/open-findings.md`,
which this stream does not own. See `docs/scenarios/proc.md` for four more,
including one — the session tick — that would change how any multi-source
`runtime` session behaves.

Each has a test in `tests/scenario_runtime.rs` named `defect_<n>_...` that
asserts the *wrong* behaviour on purpose. When one starts failing, the defect is
fixed: delete the test and the entry here together.

### 1. `topk` ranks processes, not the methods that were compiled

`topk` ranks *series*, and a series is a distinct set of keys. `runtime.jit`
carries one key, `task`, so a run answers `topk:10` with exactly one row per
process however many methods were compiled — and that row's total is the event
count, which `summary` already gives.

```console
$ omn runtime.jit --mode trace --emit topk -- sh -c '<writes 5 entries>'
  5 events over 110.73ms, stopped because the launched program exited with exit status 0

  topk: the 1 heaviest of 1 series
    (10 were asked for and the run produced 1)
     1. task:3454932      5 count  over 5 events
    nothing was left out: that is every series
```

Five distinct methods were compiled and the ranking names none of them. The
question a JIT map exists to answer is *which methods dominated*, and `method`
is an attribute rather than a key, so no emit can group on it.

This is a promise that came due. `examples/runtime-jit.yml` said in an earlier
wave that "a profile of *which* methods cost the most needs `topk`, which lands
with the shared aggregation work". The aggregation landed; the question is still
unanswerable. Either `topk` needs to be able to group by an attribute, or
`runtime.jit` needs `method` as a key — and the second is the larger change,
since a key is what joins across sources.

The comment in `runtime-jit.yml` has been rewritten to say what is true now, and
`topk` is deliberately absent from that session rather than present and
misleading.

### 2. `=~` is a substring test, and nothing says so

The filter grammar's `=~` is `str::contains`. The manual lists it among the
operators and never says what it does:

```troff
Supports
.BR == " " != " " < " " <= " " > " " >= " " =~ " and " in ,
```

Everything else that spells a match operator this way — Perl, Ruby, awk, bash's
`[[ =~ ]]` — means a regular expression. A predicate written on that assumption
is parsed, validated, reported as applied, and keeps nothing:

```console
$ omn runtime.jit --filter "method =~ 'omniscientSwept'" ...   # keeps 4 of 4
$ omn runtime.jit --filter "method =~ 'Swept[1-4]'" ...        # keeps 0 of 4
```

Same silent-empty failure mode as the `kind`/`code_kind` collision this family
already had, and the same cost: an empty series looks exactly like a workload
that did nothing. The run *does* report `kept 0 of 4`, which is the thing that
makes it recoverable rather than invisible — but only if the reader notices, and
`0 of 4` is also what a correct filter over a quiet workload prints.

Cheapest fix is one line of manual: say it is a substring test. The better one is
to refuse a pattern containing regex metacharacters that cannot match literally,
since `[`, `(` and `\` almost never appear in a symbol name and always appear in
a misread predicate.
