# `proc` — what is worth measuring, and how you know the answer is right

All six sources of this family collect. `proc.fd` and `proc.threads` read two
counts a process keeps about itself, over procfs, on a clock; `proc.exec`,
`proc.exit`, `proc.signals` and `proc.syscall` read events the kernel publishes
as they happen, over raw tracepoints, because there is no file to poll for any
of them and a poll would answer with whatever was under the clock.

The first two are the cheapest honest ground truth in the catalog. Everything else in
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
| What ran on this machine, and what became of it? | `proc-lifecycle.yml` | a copy of `/bin/sh` under a name the test invented, executed a chosen number of times, told to `exit 7` once and killed with `SIGKILL` once |
| Where did the kernel time go? | `proc-syscall.yml` | exactly 1000 `getppid` calls, and ten sleeps of a delay this session injected |
| Does every axis the event sources declare work? | `proc-bpf-sweep.yml`, `proc-syscall-sweep.yml` | every emit and every filter field each of the four declares, in two files because one run observes one target |
| When the ring overflows, does it say so -- and by how much? | `proc-signal-storm.yml` | 300000 real-time signals sent by the workload; `counted + dropped` has to close against that |
| Does every axis the catalog declares work? | `proc-sweep.yml` | the same workload, through every emit, every structured format and the one declared filter field |
| What is it holding right now? | `proc-one-reading.yml` | `snapshot`: one read, and the only emits `snapshot` can carry |
| Did the poller keep up, and does it say so? | `proc-loss.yml` | the program opens exactly 30000 descriptors and reports `OPEN 30003`; a 30000-entry directory cannot be walked in the 1 ms tick the session asks for |
| Is a program that ends a program that lost data? | `proc-exit.yml` | it holds still for 150 ms and exits 0, against a session asking for 2 s |
| Are the three refusals the right three? | `proc-not-built.yml`, and the session below | exit 2 for a request that cannot mean anything, exit 3 for one this build has not written (1 where tracefs is root-only: see below) |

### A — the capability sweep

The two procfs sources declare two modes, three emits, and `comm` as a filter
field. `proc-sweep.yml` covers the `poll` half and `proc-one-reading.yml` the
`snapshot` half, in two files rather than one, because a session that mixes them
does not measure what it says — defect 3.

The four event sources are swept in two more files, and for a different reason:
**one run observes one target.** `proc.exec`, `proc.exit` and `proc.signals`
declare no `launch` target and `proc.syscall` does, so `proc-bpf-sweep.yml`
takes the machine-wide three and `proc-syscall-sweep.yml` takes the fourth with
its own workload. Between them every emit and every filter field each of the
four declares is asked for:

| Source | Emits | Filter fields |
|---|---|---|
| `proc.exec` | `raw`, `timeline`, `topk`, `summary` | `comm`, `filename` |
| `proc.exit` | `raw`, `timeline`, `topk`, `summary` | `comm`, `code` |
| `proc.signals` | `raw`, `topk`, `rate`, `summary` | `comm`, `sig`, `pid` |
| `proc.syscall` | `hist`, `topk`, `rate`, `raw`, `summary` | `comm`, `syscall`, `pid` |

A machine-wide sweep has a problem the launched ones do not: what it finds is
whatever the machine was doing, and on a quiet machine that is nothing at all —
an empty `topk` that passes. So the test starts a workload of its own while the
run is open, a copy of `/bin/sh` under a name it invented, and requires each
emit to be non-empty. That is the same device `examples/cpu-one-busy-core.yml`
documents for a pinned spin loop.

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

**This family still contributes nothing to class B, and the honest thing is to
say so rather than invent a comparison.** `proc.fd` and `proc.threads` have one
backend each, `fs:proc`. The four event sources have two each — `bpf:raw_tp`
ahead of `bpf:tp` — and only the first of each pair is implemented, so there is
still no chain anywhere in `proc` with two roads to compare.

That is deliberate rather than unfinished, and the reason is worth stating so
that nobody writes the second one to fill the column. The two backends of each
chain see **exactly the same events**; the catalog says so with
`equivalent: true`. What differs is only what they need to attach: a raw
tracepoint needs a name and the kernel's own BTF, and a classic one needs a
numeric event id out of `<tracefs>/events/<category>/<name>/id`, which is
`-r--r----- root root` on a stock machine. A backend that is strictly more
demanding and answers identically is a fallback for machines without BTF, not a
road worth benchmarking.

What is checked instead is that the roads are named and that pinning behaves:
`--backend force:fs:proc` measures and says twice that the chain was not walked,
and `--backend force:bpf:tp` is refused by name — which is now class D's
demonstration as well, below.

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

**The event sources lose data the other way, and `proc-signal-storm.yml` is the
only place in this family where a loss closes against a number.** A ring-backed
source has no clock to run late; what it has is a 256 KiB ring drained every ten
milliseconds and a program that counts what it could not publish, because the
BPF ring buffer does not report its own overflows. The workload sends itself
three hundred thousand real-time signals — real-time signals *queue*, so every
one of them is generated and every one reaches the program — at roughly half a
million a second, which is more than a drain interval holds.

What must then be true is an identity rather than a threshold:

```
counted + dropped  >=  sent
```

because a signal that was generated was either counted or dropped, and the drop
counter is machine-wide so it also holds whatever else the machine lost in the
same window. Measured here: 291 169 counted and 8 832 dropped against 300 000
sent, with the reason collapsed into one line — `44x the signal ring was full`.

And on a machine fast enough to keep up, `dropped` is zero and `counted` is
exactly `sent`. **Both readings are asserted**, because asserting that this
machine must lose something would be asserting the machine — finding 35's
mistake in a new place. What the scenario catches is the third reading, which is
a run that reported fewer signals than were sent and called itself clean.

The loss must also be a drop and not a late tick, and the test asserts that too:
the two are different facts and this source can only have one of them.

### D — the refusal

Three answers, three exit codes, and the distinction is the point: a caller that
cannot tell "you asked for something impossible" from "this is not written yet"
cannot decide whether to fix the request or wait for a release.

| It says | It means | Exit | Reachable here |
|---|---|---|---|
| a rejection | the request cannot mean anything | 2 | the session below |
| no collector | this build does not implement it | 3 | `proc-not-built.yml`, where tracefs is readable |
| no viable backend | this machine cannot serve it | 1 | through `--catalog`, in the test |

**This row has now been all three things, and the sequence is the point.**

It began as exit 3: the four event sources had no collector, they declared
`bpf:tp` and nothing else, and the resolver selected that backend because the
probe for a `tracepoint:` requirement answered by listing the event's
*directory* under tracefs, which uid 1000 may traverse. The run then said
"nothing collects this" — and would have failed at attach, because the `id` file
the attach really reads is root-only. Finding 15.

It became exit 1 when the probe started answering from the access tracefs
actually grants. That was the tool becoming more truthful and the demonstration
getting worse: the family could no longer show exit 3 at all on this machine,
and the scenario had to accept either refusal, which is a test that asserts the
machine rather than the build.

It is exit 3 again, and deterministically, since the sixth wave. All four
sources collect through `bpf:raw_tp` — a name and the kernel's own BTF, no
tracefs — and what is left unimplemented is the *second* backend of each chain.
`proc-not-built.yml` pins it with `backend: "force:bpf:tp"`, and `force:`
bypasses the resolver's choice.

It is not 3 everywhere, and it was wrongly written down as such until the
twentieth wave. `force:` skips the choice, not the pinned backend's own
probes, and a missing requirement is refused before a missing collector: the
session exits **3** where tracefs is readable and the binary holds its
capabilities, and **1** where tracefs is root-only, as on this project's own
machine (`GUIDE.md` says the same). The demonstration of exit 3 is therefore
still a demonstration that depends on the machine, for the reason below.

There is a lesson in that for the other families, and it is not about `proc`:
**a refusal scenario that depends on a probe is a scenario that will change
answer when the probe gets better.** Pinning the backend takes the resolver out
of the question, and the refusal being demonstrated is about the build.

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

**The uniquely named shell** (`proc-lifecycle.yml`, `proc-bpf-sweep.yml`). Not a
program but a *name*: a copy of `/bin/sh` at `/tmp/omnx<pid>x<n>`, made by the
test and removed after it. Guarantees the one thing a machine-wide scenario
otherwise cannot have — an identity nothing else on the machine can accidentally
share. A count of execs on a busy machine is whatever the machine was doing; a
count of execs *of this path* is exactly what the test ran, so the assertion is
an equality rather than a lower bound.

Two details are not incidental. The name is under fifteen characters because the
kernel truncates `comm` to that, and a name cut in half is one the exit events
cannot be matched on. And it carries a serial as well as the pid, because
`cargo test` runs these in parallel threads of **one** process: without it every
test in this file asks for the same name and each counts the others' programs as
its own, which is `docs/contributing.md`'s parallel-suite trap in a new place.
It was found by the tests reporting 9 where they expected 5.

**The real-time signal storm** (`proc-signal-storm.yml`). Python. Installs a
handler for `SIGRTMIN` so the signals do not kill it, then sends itself exactly
300000 of them as fast as it can — about half a million a second, 0.65 s of
wall time. Guarantees more events per second than a 256 KiB ring drained every
ten milliseconds can hold, and a number to close the loss against. Real-time
rather than `SIGUSR1` on purpose: a non-real-time signal that is already pending
is not queued again, so a `SIGUSR1` storm generates 300000 events and delivers
one, and the count under test would have been a property of signal semantics
rather than of the ring.

**The known-syscall program** (`proc-syscall.yml`, `proc-syscall-sweep.yml`).
Python. Calls `getppid` exactly 1000 times and then sleeps 50 ms ten times,
reporting `CALLS 1000` and `SLEEPS 10 EACH 50000000`. Guarantees two independent
properties for one workload: an exact count, and an injected delay. `getppid` is
the choice it is because glibc does not cache it, a Python start-up makes none
of them, and it does almost nothing in the kernel — so its latency is the cost
of the transition and nothing else. Verified with `strace -c` before it was
written into the session: 1000 `getppid` and 10 `clock_nanosleep`, and no others
of either.

---

## What this family contributes to a crossing

`proc` knows **how many handles and threads a process holds over time**, which
no other family can see: `mem` knows what it mapped, `io` knows what it wrote,
and neither can say how many descriptors it took to do it. The key that carries
it is `task`, and every session in this family sets `keys: [task]`.

The crossing it is for is a leak: a descriptor count that climbs while
`mem.rss` climbs with it is a program holding sockets or files open, and the two
series separate that from a program simply allocating.

Three more crossings became possible in the sixth wave, and none is dispatched:

- **`proc.exec` and `proc.exit` bound every other series in time.** A program
  that lived for four milliseconds is invisible to every poller in this tool and
  is two events here, so a `mem.rss` series with nothing in it and an exec/exit
  pair four milliseconds apart are the same fact told twice. The key is `task`.
- **`proc.syscall` beside `io` and `net`.** A `read` that took forty
  milliseconds and a block request that took forty milliseconds are the same
  wait seen from the two ends of the kernel, and until `io.latency` has a
  producer there is no way to check that they agree.
- **`proc.signals` beside `sched.offcpu`.** A signal is one of the things that
  ends a block, and `sched.wakeup` cannot say which.

What all three still want is the `proc:pidns+pid` key rather than `task:tid`,
since a tid alone is ambiguous the moment containers exist. The key exists in
the model, nothing produces it, and producing it needs the same cgroup and
namespace helpers the cgroup target does.

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

**The `bpf:tp` half of each event source's chain.** Deliberate rather than
pending: the classic tracepoint sees exactly the same events as the raw one and
needs a root-only file to attach, so it is a fallback for a machine without BTF
and not a second road worth driving. `proc-not-built.yml` is what it is for.

**Anything about a container or cgroup target.** `proc.exec`, `proc.exit` and
`proc.signals` declare `cgroup: bpf_only` and `container: bpf_only`, and
`proc.syscall` declares both too. That is a statement about which *backend*
could serve them, not a claim that this build does: narrowing an eBPF program to
a cgroup needs a helper the shared layer does not have —
`bpf_get_current_cgroup_id`, or `bpf_current_task_under_cgroup` against a
`BPF_MAP_TYPE_CGROUP_ARRAY`. The collectors **refuse both targets by name**,
saying which helper is missing and what to use instead, which is what
`sched.wakeup` has done since wave two. The request is in
`docs/open-findings.md`.

**The syscall table on any architecture but x86-64.** Syscall numbers are not
portable — 1 is `write` here and `exit` on arm64 — and
`src/collect/proc/syscall_names.rs` carries one architecture's table. Elsewhere
the event reports `syscall_<n>` and a predicate written against a *name* is
refused rather than silently matching nothing. Nothing here can be an arm64
machine; adding one is one array and one `cfg`.

**`proc.syscall`'s `req` and `file` keys, and the `cgroup` key on `proc.exec`
and `proc.exit`.** The catalog declares them and no event carries one, which is
exactly the shape of defect 5 below. It belongs to whoever owns
`catalog/*.yaml`; the request is in `docs/open-findings.md`.

**Which process generated a signal, as opposed to which was running.**
`signal_generate` fires in the context of whoever raised the signal, so
`bpf_get_current_pid_tgid` is the sender — but a signal raised by the kernel
itself, for a timer, a fault or a dying child, is credited to whatever was
interrupted. That is the same caveat `sched.wakeup` carries about its waker, and
`sender_pid` is evidence rather than proof.

**The exact thread count immediately after a program starts its workers.** The
catalog caveat says a thread appears in procfs once it is running, not once the
call that created it returns, so a poll taken immediately after start sees fewer
than were started. The workloads here report their count *after* starting the
workers and then hold still, which sidesteps it rather than measuring it. A
scenario that measured the ramp would need to know when each thread began, which
is `sched` territory.

---

## Defects found

Six, of which **two are fixed** — 1 and 2, by stream E of the fifth wave. Three
of the remaining four (3, 4, 6) are in the shared layer rather than in this
family and were found through it; they are the coordinator's to route into
`docs/open-findings.md`, which the stream that found them did not own.

Each has a test in `tests/scenario_proc.rs` named `defect_<n>_...`, which began
by asserting the *wrong* behaviour on purpose: the stream that found them could
not edit `src/`, and a defect recorded only in prose is one that gets fixed
silently or not at all.

**All six are fixed** — 1 and 2 by stream E of the fifth wave, 3, 4 and 6 by
stream A, and 5 by stream B. The convention that replaced "delete the test" is
`BRIEF.md`'s: **the reproduction stays and what it asserts is turned round**, so
the scenario that found a defect is the one that would fail if it came back.
Each entry below says which test carries it, and `docs/fixes/` says what became
of it.

**The sixth wave found three more, 7 to 9, and all three are in layers this
family does not own.** They are written up at the end of this section and
requested, with the exact change wanted, under `## Needs from another stream` in
`docs/open-findings.md`.


### 1. The first reading of a launched target described this tool, not the program

**Fixed** — `src/collect/proc/lifecycle.rs` and `src/collect/proc/status.rs`.

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

**It is a race with three outcomes, not two.** Under the full suite the same
session reported `comm=python3` with `open=3` on about one run in five: the
child had reached its `execve` and had not yet opened its files. So `comm` is
not the tell it looks like — it says which image the pid is running, not whether
the program has done anything.

That makes the defect larger than it first read, not smaller. A measurement that
is reliably wrong can be worked around; one whose subject depends on scheduling
cannot. The test asserts exactly that: over five runs of the same session
against the same program, at least one answer must differ from what the program
holds. A snapshot that always found twenty would be a reliable measurement, and
the test would be telling the truth by failing.

A filter hides it. `proc-sweep.yml` carries `comm =~ 'python'`, which removes
the pre-exec reading because that reading says `omn` — so a session written to
select a program silently repairs this and a session without one does not. That
is worse than the defect, because it makes the defect intermittent across
sessions that look equivalent.

### 2. The last reading of a launched target reported a zombie as holding nothing

**Fixed** — `src/collect/proc/lifecycle.rs` and `src/collect/proc/status.rs`.

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

**The fix** is the second of those: the collector reads the process state out of
`/proc/<pid>/stat` -- from the last `)` rather than by counting fields, because
a program may call itself `a b) c` -- and reports nothing for a target in `Z` or
`X`.

It reports nothing for one round rather than ending the run there, and that is
deliberate. The runner asks its own stop condition after each round, and for a
launched program that condition has the better reason: it knows the exit status,
so the run still says `stopped because the launched program exited with exit
status 0` rather than losing it to the collector's wording. A `--pid` target
that nothing else is watching reaches the round after and is ended by the
collector.

`examples/proc-exit.yml` reported `min 0, mean 12.6, max 20, last 0` and now
reports `min 4, mean 16, max 20, last 20`. Asserted by
`defect_2_the_last_reading_of_a_launched_target_is_one_the_program_was_alive_for`,
which checks the descriptor series by value and the thread series by point
count -- a zombie's `Threads:` line says one, which is indistinguishable by
value from the single-threaded program that was there a moment earlier, but the
round that read it added a point to both series.

### 3. A session takes its tick from the first request and discards the others

> **Fixed** (stream A). Each request is driven by its own `Pace`, from its own
> mode and its own collector; a snapshot is one read wherever it is written; and
> an interval of zero no longer becomes the interval of everything else. The
> reproduction is kept and `every_request_in_a_session_keeps_the_rate_it_asked_for`
> asserts the other answer for all three of its faces.

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

> **Fixed** (stream A). Exit 1, from `exit_for` in `src/main.rs`, which is the one
> place the four answers map to codes. Asserted by
> `a_machine_that_cannot_serve_a_valid_request_has_its_own_exit_code`.

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

**Fixed** by stream B of the fifth wave; see `docs/fixes/catalog.md`. The entry
declares `keys: [task]`, so the promise and the events agree and the request is
refused before anything is attached:

```console
$ omn proc.fd --mode poll@50ms --emit raw --keys task,file -- true
error: [proc.fd #1] `proc.fd` does not carry the `file` key, so it cannot be joined on it
  hint: `proc.fd` carries task
```

The other way out — one event per descriptor, carrying a `file` key — is what
this entry's own caveat about walk cost refuses, and the entry now says so, so
that the next reader does not have to rediscover the trade. The scenario is
`the_only_key_proc_fd_declares_is_the_only_one_its_events_carry`, which
replaces `defect_5_...` and asserts both halves: the refusal, and that the
events carry exactly the key that is left. What was measured:

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
should not declare it. The entry no longer declares it.

### 6. A late-tick reason carries its own nanosecond count, so losses never deduplicate

> **Fixed** (stream A). The reason is the shape and the counters carry the
> magnitudes, which is what this entry asks for: `losses.overrun` holds the tick,
> the worst overrun and the total, and the one reason carries the count it
> explains. Asserted by
> `a_late_tick_is_one_reason_with_a_count_rather_than_one_reason_each`.

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

---

### 7. A collector's notes are printed under the word "degraded"

Found while landing the four event sources; belongs to `src/emit.rs` and
`src/encode.rs`.

`Collector::notes()` is how a collector qualifies its own numbers, and the emit
layer folds those notes into `degradations`, which `src/encode.rs` prints as
`degraded: <note>`. A backend that fell back to a lesser mechanism and a
collector stating a fact about its measurement then read identically:

```console
$ omn proc.syscall --system -m trace -e hist -d 1s
  degraded: machine-wide: every syscall on the machine, including the ones this
            tool makes to drain the histogram
  degraded: only `getppid` (110) was timed: the predicate on `syscall` was pushed
            into the program, so no other syscall is in these numbers at all
```

Neither of those is a degradation. The second is the tool doing exactly what the
catalog asked of it, reported as though something had gone wrong.

It is small, and it is this project's own class of defect: the output telling a
reader something that is not true. It was worked around here by wording every
note so that it reads as a qualification of the measurement — which is why the
filter note leads with *"only `getppid` was timed"* rather than with *"the
filter ran in the kernel"* — and a wording that has to be chosen to survive its
own label is a wording that will drift.

**What would close it.** Two lists rather than one: a degradation is something
the run had to give up, a note is something the reader has to know. They print
under different words. `sched.runqlat` has carried the same confusion since wave
three and nobody had named it.

### 8. `map_create` refuses with a bare errno, before any advice can reach it

Found on an unprivileged run; belongs to `src/bpf/syscall.rs` or `src/bpf/map.rs`.

Every program in the eBPF layer attaches `permission_advice` to a *program load*
that fails on permission grounds, and that message is good: it names `CAP_BPF`,
`CAP_PERFMON`, the `setcap` line and the sysctl that most often gets in the way.
But a program load is not the first privileged thing a collector does. **Map
creation is**, and it fails first:

```
skipped: cannot create the `sched_process_exec` ring buffer:
         Operation not permitted (os error 1)
```

That is what somebody without the capabilities actually sees, and it names
neither the capability nor the way to get it. It is the same for `wakeup.rs`,
`offcpu.rs`, `runqlat.rs` and `migrate.rs`, which have shipped it since wave
two; nobody noticed because the message only appears on a machine without the
capabilities, and the development machine has them.

This family wraps it locally — `map_failure` in `src/bpf/proc_events.rs` and
`src/bpf/proc_syscall.rs` turns `EPERM` into the same advice — but the wrapper
belongs one layer down, where every program would get it.

### 9. Three key declarations the catalog makes and no event carries

Belongs to whoever owns `catalog/*.yaml`. It is defect 5 exactly, in three new
places:

| Entry | Declares | Events carry |
|---|---|---|
| `proc.syscall` | `keys: [task, req, file]` | `task` |
| `proc.exec` | `keys: [task, cgroup]` | `task` |
| `proc.exit` | `keys: [task, cgroup]` | `task` |

A join written on `file` or `cgroup` matches nothing and reports that it
attributed zero events, which reads as a workload that did nothing rather than
as a key that was never there. The validator will accept `--keys cgroup` and
tell a reader that the source carries it.

`req` and `file` on `proc.syscall` look like the remains of a design where one
event was one syscall with its arguments decoded; this collector aggregates in
the kernel and cannot carry either, and the entry's own caveat is why. `cgroup`
on the other two waits on the same helper the cgroup *target* does, so it is
worth keeping in the entry only if the entry is meant to describe what a future
build would emit — and `CATALOG.md` says the opposite, that a declaration is a
promise the build keeps.

### 10. `proc.syscall`'s `comm` was the name its window's first call saw

Found by the twenty-second wave's bpftrace pair (gap 1), **fixed in the
twenty-fourth**. The kernel's row for a `(task, syscall, bucket)` was created
at the window's first call and stored the task's name then, once; every later
call of that window was counted under it. A task that renamed itself
(`prctl(PR_SET_NAME)`) kept its old name on every call it had already made
once in the window, and `comm == "<new name>"` dropped them: 500 opens after a
rename counted 1. The name is now part of the row's key, read on every call,
so a rename starts new rows. Held by
`a_task_that_renames_itself_is_counted_under_each_name_with_its_errors`: 300
failing opens under `python3`, the rename, then 300 opens and 300 failing
ones, read 600 calls and 300 errors under the new name, exactly.

## What the twenty-fourth wave added

Every number below was read on this machine against something independent of
`omn`.

- **`proc.syscall` keyed `syscall` and `comm`, with `errors`.** Every row
  carries `task`, `syscall` and `comm` keys, so `-k syscall` is one run's
  `strace -c` table and `-k comm` the bpftrace tools' per-name one; each row
  counts the calls that returned an errno (`errors`, `strace -c`'s column).
  The workload above, keyed `comm,syscall`: `comm:omnref-open syscall:openat
  n=1000` (its 500 opens and 500 failures) and `comm:python3 syscall:openat
  n=548`; `errors` summed over `-e raw` rows: 500 and 506 (the 500 chosen
  failures and six of python's import probes). No aggregating emit sums
  `errors` yet; it is per row.
- **`proc.calls`, one record per call** (`src/bpf/proc_calls.rs`): when it
  began, `lat_ns`, `ret`, `errno`, the task and its name, and `openat`'s
  `path` and `fd`. Against `strace -f -e trace=openat` on the same workload:
  648 opens and 406 ENOENT both ways. Held by
  `every_open_is_one_record_with_its_path_its_errno_and_its_descriptor`.
- **`proc.tasks`, the machine's processes or every thread of one**, on
  `fs:proc` (`/proc/<pid>/stat`, `statm` for the resident set). `busy` is the
  share of one cpu since the previous poll, `top`'s `%CPU` over a hundred. A C
  workload of three spinning threads and one napping read 0.973-1.000 per
  spinner and 0.000 for the napper; the same process at `--system` read 2.979
  where `top -b` read 298.4 %. `statm`, not `stat`'s field, is read for the
  resident set: one small process read 642 pages there and 568 in `stat`,
  and `top` shows `statm`'s. Held by the three `proc.tasks` tests of
  `tests/family_proc.rs` (the thread count exactly, a spinning against a
  waiting thread, a process this test started found once by `--system`).
- **`proc.exec` per launch**: each exec carries its parent (`ppid`, read in
  the kernel), and on a pid or launched target the run keeps the program's
  execs and its descendants'. A script of five `echo`s, a subshell and a
  pipeline read its ten execs, three runs of three, beside a loop of
  `/bin/true` that added none. Held by
  `a_launched_programs_execs_are_its_tree_and_nothing_beside_it`.
- **`proc.signals`' sender**: `sender_comm` beside `sender_pid`, and `comm` as
  a key on `proc.exec`, `proc.exit` and `proc.signals`. Three kills of a
  `sleep` read `sender_comm` `kill` and the kill's pid.
