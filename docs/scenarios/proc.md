# `proc` — what is worth measuring, and how you know the answer is right

Two sources of this family collect: `proc.fd` and `proc.threads`, both over
procfs. Four do not, and all four wait on the same piece of work.

They are the cheapest honest ground truth in the catalog. Everything else in
this tool is measured with skid, with sampling, or with a folio size in the way;
a process holding twenty descriptors holds twenty. So every scenario here
asserts an exact equality rather than a tolerance, and the workload is a program
that counts its own descriptors and threads and writes the numbers to standard
error before holding still. The comparison is between two independent readers of
the same kernel state — the measured process from inside, this tool from
outside — and a scenario that compared the tool with itself would prove nothing.

Sessions are in `examples/proc-*.yml`, checks in `tests/scenario_proc.rs`. Every
test runs one of those files, so anything below can be reproduced by hand.

---

## Scenarios

| Question | Session | What makes the answer checkable |
|---|---|---|
| Is it leaking handles or threads? | `proc-handles.yml` | the program opens 17 files on top of its 3 and runs 5 workers beside its main thread, and reports `HELD 20` / `RUNNING 6` |
| Does every axis the catalog declares work? | `proc-sweep.yml` | the same workload, through every emit, every structured format and the one declared filter field |
| What is it holding right now? | `proc-one-reading.yml` | `snapshot`: one read, and the only emits `snapshot` can carry |
| Did the poller keep up, and does it say so? | `proc-loss.yml` | the program opens exactly 30000 descriptors and reports `OPEN 30003`; a 30000-entry directory cannot be walked in the 1 ms tick the session asks for |
| Is a program that ends a program that lost data? | `proc-exit.yml` | it holds still for 150 ms and exits 0, against a session asking for 2 s |
| Are the three refusals the right three? | `proc-not-built.yml`, and the session below | exit 2 for a request that cannot mean anything, exit 3 for one this build has not written |

### A — the capability sweep

Both sources declare two modes, three emits, and `comm` as a filter field.
`proc-sweep.yml` covers the `poll` half and `proc-one-reading.yml` the
`snapshot` half, in two files rather than one, because a session that mixes them
does not measure what it says — defect 3.

Every structured format (`json`, `yaml`, `toml`, `ndjson`) carries all three
emits. The stack formats do not and must not: `folded`, `pprof` and `speedscope`
refuse `timeline`, `raw` and `summary` by name and exit 2, and `chrome` carries
a timeline and refuses a summary. A format that claimed an emit it cannot carry
would write an empty profile that looks like a workload that did nothing.

Two refusals inside the sweep are correct and worth naming, because they are the
axis interaction working:

- `snapshot` with `timeline` is refused — a series of one point is not a series.
- `count` with `topk` is refused on the unimplemented sources — a mode that
  never sees an individual event cannot rank who produced them.

### B — the backend comparison

**This family contributes nothing to class B, and the honest thing is to say so
rather than invent a comparison.** `proc.fd` and `proc.threads` have one backend
each, `fs:proc`, and the other four sources have one each, `bpf:tp`. There is no
chain anywhere in `proc` with two implemented backends, and none with two
backends at all.

What is checked instead is that the single road is named and that pinning it
behaves: `--backend force:fs:proc` measures and says twice that the chain was
not walked, and `--backend force:bpf:tp` is refused with the chain printed
rather than accepted and ignored. The effort that class B would have taken went
to class D, as `BRIEF.md` directs.

### C — the loss path

`proc.fd` has no ring to overflow and no kernel counter to be throttled. Its way
of losing data is quieter: there is no cheap count of open descriptors —
`FDSize` in `status` is the size of the allocated table, not the number in use —
so the directory is walked, and the walk costs one entry's work per descriptor.
A process holding thirty thousand cannot be read in a millisecond.

`proc-loss.yml` asks for a tick the read cannot meet, on purpose. The run must
report 30003 at its peak *and* count 77-ish late ticks, and it must not report
them as dropped events: a late tick is a series whose spacing is not what it
claims, a dropped event is a measurement that happened and was not recorded, and
only the first is available to this source.

`proc-exit.yml` is the counterexample, and it exists so the two cannot be
confused. procfs answers a read of a departed process with ESRCH while it is
going and ENOENT once it is gone; either means the target left, which ends the
run cleanly with every loss counter at zero. Counting either as a lost read
would make every clean run look lossy. There is a third answer procfs gives that
the collector does not handle, and it is defect 2.

### D — the refusal

Three answers, three exit codes, and the distinction is the point: a caller that
cannot tell "you asked for something impossible" from "this is not written yet"
cannot decide whether to fix the request or wait for a release.

| It says | It means | Exit | Reachable here |
|---|---|---|---|
| a rejection | the request cannot mean anything | 2 | the session below |
| no collector | this build does not implement it | 3 | `proc-not-built.yml` |
| no viable backend | this machine cannot serve it | 1 | **no** — see defect 4 |

`proc-not-built.yml` validates cleanly, resolves onto `bpf:tp`, then exits 3
naming the backend, the source and `--dump-request` as the thing a reader can
still do with a valid request.

The rejections could not be shipped as an example, and the reason is worth
recording for the other seven streams: `tests/cli.rs` walks `examples/` and
requires every session in it to validate, so a session that is invalid by
construction cannot live there. It lives in `tests/scenario_proc.rs` as a
constant, and here, so a reader still has it:

```yaml
# omn validate -f <this>   ->   exit 2, three reasons
name: three ways a request cannot mean anything
format: text

requests:
  # A descriptor table belongs to a process. The machine has no descriptor
  # count, so `--system` is not a harder measurement, it is a different
  # sentence.
  - label: no-such-target
    source: proc.fd
    target: system
    mode: poll@100ms
    emit: [raw]

  # `proc.fd` is a counter read on a clock, not an event stream. There is no
  # descriptor-open event for `trace` to follow -- that is `proc.syscall`, a
  # different source with a different mechanism.
  - label: no-such-mode
    source: proc.fd
    target: {launch: [sleep, "0.1"]}
    mode: trace
    emit: [raw]

  # A histogram of a polled gauge is a histogram of when the poller happened to
  # look, which is a property of the tick and not of the program.
  - label: no-such-emit
    source: proc.threads
    target: {launch: [sleep, "0.1"]}
    mode: poll@100ms
    emit: [hist]
```

`validate` answers for all three at once instead of stopping at the first, which
is what a reader fixing a session needs. Each names what is actually missing
*and* what would have worked: the targets this source accepts, the modes it has,
the emits it declares.

---

## Workloads

Phase two reuses these. What each one guarantees:

**The handle-and-thread program** (`proc-handles.yml`, `proc-sweep.yml`,
`proc-exit.yml`, `proc-one-reading.yml`). Python. Opens 17 files on top of the
three every process starts with and starts 5 worker threads that block on an
event, writes `HELD 20` and `RUNNING 6` to standard error, holds for 300 ms
(150 ms in `proc-exit.yml`), then releases the workers and exits 0. Guarantees:
a known, constant descriptor count and thread count for the whole of its
measured life, and a clean exit at a chosen moment. It is entirely blocked, not
half on-cpu — P2 needs a program that is both, and this is not it.

**The thirty-thousand-handle program** (`proc-loss.yml`). Python. Raises its own
`RLIMIT_NOFILE` soft limit to the hard limit, opens exactly 30000 descriptors,
reports `OPEN 30003`, holds for 300 ms. Guarantees: a procfs read expensive
enough to overrun any tick under about 15 ms, which is the only way to make this
family lose anything. Costs about 0.75 s, nearly all of it python opening files
— the slowest scenario in the family, and the reason is worth knowing before
copying it.

---

## What this family contributes to a crossing

`proc` knows **how many handles and threads a process holds over time**, which
no other family can see: `mem` knows what it mapped, `io` knows what it wrote,
and neither can say how many descriptors it took to do it. The key that carries
it is `task`, and every session in this family sets `keys: [task]`.

The crossing it is for is a leak: a descriptor count that climbs while
`mem.rss` climbs with it is a program holding sockets or files open, and the two
series separate that from a program simply allocating. `proc.exec` and
`proc.exit` would carry far more — the `proc:pidns+pid` key rather than
`task:tid`, since a tid alone is ambiguous the moment containers exist — and
they wait on `bpf:tp`.

---

## What cannot be verified here

**Class B, at all.** No `proc` source has two backends. Stated above; not a
machine limit, a catalog fact.

**Exit 1.** No `proc` or `runtime` source can fail to resolve on a real Linux
machine: the implemented backends need only procfs, and the unimplemented ones
refuse with exit 3 first. The scenario for it therefore goes through `--catalog`
with a copy of the catalog in which `proc.fd` requires a file that cannot exist
— which is how defect 4 was found. A machine with `/proc` unmounted would reach
it for real, and nothing here can be that machine.

**The four unimplemented sources' measurements.** `proc.syscall`, `proc.exec`,
`proc.exit` and `proc.signals` have sessions in `proc-not-built.yml` and no
numbers, because there is no collector. Their *refusal* is checked; their
accuracy cannot be. The tracepoints they name are present on this machine —
`omn list proc` says `yes` in HERE and `none` in COLLECTOR, two different
questions kept deliberately apart — so this is a build limit, not a machine one,
and the day `bpf:tp` lands the file becomes a session.

**Anything about a container or cgroup target.** `proc.exec`, `proc.exit` and
`proc.signals` declare `cgroup: bpf_only` and `container: bpf_only`. Both wait
on the same eBPF work.

**The exact thread count immediately after a program starts its workers.** The
catalog caveat says a thread appears in procfs once it is running, not once the
call that created it returns, so a poll taken immediately after start sees fewer
than were started. The workloads here report their count *after* starting the
workers and then hold still, which sidesteps it rather than measuring it. A
scenario that measured the ramp would need to know when each thread began, which
is `sched` territory.

---

## Defects found

Six. Three of them (3, 4, 6) are in the shared layer rather than in this family,
and were found through it; they are the coordinator's to route into
`docs/open-findings.md`, which this stream does not own.

Each has a test in `tests/scenario_proc.rs` named `defect_<n>_...` that asserts
the *wrong* behaviour on purpose. This stream may not edit `src/`, and a defect
recorded only in prose is one that gets fixed silently or not at all. When one
of those tests starts failing, the defect is fixed: delete the test and the
entry here together.

### 1. The first reading of a launched target describes this tool, not the program

The runner releases the child from its gate and reads immediately. The child has
not reached its `execve` yet, so `/proc/<pid>/fd` is still the forked image of
`omn`: the pid is right and everything else is wrong.

```console
$ omn -f examples/proc-one-reading.yml
HELD 20        # what the program holds, from the program
descriptors  via fs:proc  (proc.fd)
  1 events over 354286ns, stopped because snapshot: a single read
  no loss: every read succeeded and no tick ran late

  raw
    79431307978969  proc.fd  task:3993362  open=3 limit=524288 headroom=524285 comm=omn
```

`comm=omn` is the tell. In `poll` mode it is merely the first point of several
and the series recovers on the second — `proc-handles.yml` reads `3, 20, 20, 20,
20, 20, 20, 0`. In `snapshot` mode that reading is the only one there is, so
`--mode snapshot` against a launched program is unusable today; read it against
`--pid` instead.

It is the same trap `STATE.md` already records for the symbolizer, in a new
place: for a launched program, `start` is too early because the child has not
exec'd. The first tick is the right moment there, and something like it is the
right moment here.

A filter hides it. `proc-sweep.yml` carries `comm =~ 'python'`, which removes
the pre-exec reading because that reading says `omn` — so a session written to
select a program silently repairs this and a session without one does not. That
is worse than the defect, because it makes the defect intermittent across
sessions that look equivalent.

### 2. The last reading of a launched target reports a zombie as holding nothing

A process that has exited but has not been reaped still has a `/proc/<pid>/fd`,
and it is an empty directory that reads *successfully* — neither the ESRCH nor
the ENOENT that `classify()` treats as "the target left". So the run takes one
more sample and records zero open descriptors, with the whole limit reported as
headroom, no loss, and a clean bill of health, for a program that never closed a
handle in its life.

```console
$ omn -f examples/proc-exit.yml --format json | jq '.results[0].outputs[2].series[0]'
{ "key": "task:...", "count": 5, "min": 0.0, "max": 20.0, "mean": 12.6, "last": 0.0 }
```

`min` and `last` are both that zero. `mean` is dragged by it. Only `max` is
usable, which is why every assertion in `tests/scenario_proc.rs` that checks a
count against the workload checks `max` — and a user reading `last` off a
`summary` to answer "how many was it holding when it finished" gets zero, every
time, for every program.

`proc.threads` has the same shape with a different number: it reads `1` from a
zombie's `status`, not `0`, because the `Threads:` line survives.

The fix is a third case in `classify()`, or a check on the process state, and
this stream may not write it.

### 3. A session takes its tick from the first request and discards the others

`run_all_within` computes one interval for the whole session: the minimum tick
the collectors declare, and where none do — these pollers declare none — the
mode of `resolved[0]`, the **first** request. Every other request's `mode` is
parsed, validated, printed back by `--dump-request`, and then ignored.

Three faces, all silent:

```console
# poll@200ms first, poll@50ms second: both run at 200ms.
first  events_seen= 3
second events_seen= 3

# poll@50ms first, snapshot second: the snapshot becomes a series.
first  events_seen= 8
second events_seen= 8

# snapshot first, poll@50ms second: Mode::Snapshot falls back to a ZERO
# interval, which is then applied to everything.
first  events_seen= 5257
second events_seen= 5257
```

That last one is a 300 ms busy loop against procfs for every source in the
session — 5257 rounds where the file asked for 7 — and it is reported as a
normal run with no loss and no warning.

This is the standing invariant that an invalid combination is refused with the
reason and never degraded silently, and it is not even a degradation: it is
silent substitution of a different measurement. Either a session must refuse
mixed modes, or each collector must be driven on its own schedule. Two requests
that ask for different rates is not an exotic session — it is the normal way to
put a cheap poller beside an expensive one.

### 4. "This machine cannot serve it" exits 2, and there is no exit 1

`BRIEF.md` defines three answers and three exit codes. Only two of the codes
exist. `src/main.rs:162` returns `EXIT_REJECTED` — 2, the code that means the
request cannot mean anything — when the resolver finds no viable backend, one
line after printing a hint that says the exact opposite:

```console
$ omn proc.fd --catalog <catalog requiring an absent file> --mode poll@50ms --emit raw -- true
error: `proc.fd` has no viable backend on this machine:
  fs:proc: /proc/omniscient-no-such-file: missing: /proc/omniscient-no-such-file does not exist
  hint: the request is valid; it is this machine that cannot serve it. Use --no-resolve to write it down for one that can.
$ echo $?
2
```

The message is right and the code contradicts it. There is no
`EXIT_NO_VIABLE_BACKEND` constant anywhere in the tree. A caller scripting
against this — the case the codes exist for — cannot tell a request it should
rewrite from one it should run on another machine, which is precisely the
distinction class D is about.

Related and already known: `STATE.md` records that "valid, but this mechanism is
not built" used to return 1 like a real failure and now returns 3. This is the
other half of that same cleanup and was missed.

### 5. `proc.fd` declares the `file` key and no event carries it

The catalog says `keys: [task, file]`. The validator accepts `--keys task,file`,
and when it refuses a key that is genuinely absent it tells the reader what is
available:

```console
$ omn proc.fd --keys mem --mode poll@50ms --emit raw -- true
error: [proc.fd #1] `proc.fd` does not carry the `mem` key, so it cannot be joined on it
  hint: `proc.fd` carries task, file
```

Every event this collector produces carries `task` alone. A join written on
`file` matches nothing and reports that it attributed zero events, which reads
as a workload that did nothing rather than as a key that was never there.

It matters past this family: phase two's premise is that every session which
could carry a key carries it, and a key the catalog promises and the build does
not emit makes that impossible to check from the catalog alone. Either the
collector should emit one event per descriptor with a `file` key — which the
caveat about walk cost suggests was considered and rejected — or the entry
should not declare it.

### 6. A late-tick reason carries its own nanosecond count, so losses never deduplicate

`Losses::note` counts identical reason strings, which is what turns a thousand
dropped events into one line saying `1000x`. The late-tick note embeds the exact
duration of the round that overran, so no two are ever equal:

```console
$ omn -f examples/proc-loss.yml
  LOSS: 0 dropped, 0 failed reads, 77 late ticks
    1x a round of reads took 1037568ns, longer than the 1000000ns tick
    1x a round of reads took 1055807ns, longer than the 1000000ns tick
    1x a round of reads took 10609983ns, longer than the 1000000ns tick
    ... 74 more
```

104 lines of output for one run, about 90 of them near-identical. Losses are
surfaced to the user always, and this is the one place where surfacing them
buries the count instead of reporting it — a reader scrolling past ninety lines
learns less than one saying `77x a round of reads outran the 1ms tick`.

The reason string should carry the shape and the counters the magnitudes: the
worst overrun is worth keeping, each individual one is not.
