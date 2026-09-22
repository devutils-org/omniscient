# `sched` scenarios

What is worth measuring in the family that measures the time a program spends
**not** running, and how you know the answer is right.

The sessions are in `examples/sched-*.yml` and the checks in
`tests/scenario_sched.rs`. Every number quoted here was read off this machine —
24 cores, kernel 7.2.6, `CAP_BPF` and `CAP_PERFMON` on the binary — and every
scenario states the property of its workload that makes the answer checkable.

Read `docs/families/sched.md` for what the sources mean; this document is about
whether they tell the truth.

---

## The one idea

A pipe ping-pong is this family's ground truth, and it is the reason every
scenario here can be checked rather than admired.

Two processes pass a byte back and forth through a pair of fifos. A round trip
is: the driver writes, blocks reading the reply, is woken, runs. The number of
round trips is written into the loop. From that one number, four different
sources have four different right answers, and none of them is derived from any
of the others:

| Source | Mechanism | What *N* round trips must be | Measured, *N* = 2000 |
|---|---|---|---|
| `sched.switches` | `perf_event_open`, one sample per switch | *N* switches out | 2011, 2014, 2018, 2035 |
| `sched.offcpu` | raw tracepoint, counted in the kernel | *N* blocks | 2001, 2002, 2002, 2003 |
| `sched.wakeup` | raw tracepoint, one record per wakeup | 2*N* wakeups | 4016, 4019, 4028, 4030 |
| `sched.runqlat` | raw tracepoint, bucketed in the kernel | *N* run queue waits | 2012, 2015, 2019, 2035 |

Four mechanisms, one workload, four numbers all within two percent of what the
loop was told to do. That is what this family's scenarios are built on, and
what makes the failures below legible: when a number is wrong here, there is
something to compare it to.

The one thing a round trip does *not* guarantee is that it blocks. If the reply
lands before the driver reaches its `read`, the driver never leaves the cpu and
nobody is woken. On an idle machine that is rare — hence the spreads above —
but under load it is common, which is why the checks allow 25 % and say so
rather than pretending to a precision the workload does not have.

---

## Scenarios

### Does every axis the catalog declares work? — `examples/sched-sweep.yml`

Every emit each of the five collected sources declares, over one workload, in
one run: `raw`, `summary`, `topk`, `timeline`, `rate` and `hist`, and both of
the two modes anything here declares.

The check does not hold a list of emits. It reads the declared set back out of
`omn list <source>` — the catalog projected — and requires the run to have
rendered each one, so a source that gains an emit gains it in the test on the
same commit. The same session through `--format json` must carry the same
emits, because a format that silently drops one is a promise broken where
nobody looks.

Two declared combinations are not in the sweep, and each is elsewhere:

- **`sched.switches --mode count`** resolves and is then refused at attach.
  `pmu:sw` samples; there is no counting path, and adding up one sample per
  switch would be a different, more expensive measurement wearing the cheap
  one's name. The refusal says exactly that and names `--mode trace`. Its exit
  code is wrong — see *Defects found*.
- **`sched.offcpu --emit stack`** works, expensively, and only for a target the
  run did not launch. `examples/sched-blocking-stack.yml`, and two defects.

Runtime: 0.26 s.

### Do two roads to the same number agree? — `examples/sched-round-trips.yml`

**This family has no backend comparison to make.** Each of its five collected
sources has exactly one implemented backend out of twenty-three declared, so
there is no second road. That is stated rather than faked, as `BRIEF.md` asks.

What there is instead is two of each other kind of comparison.

**Two mechanisms, one quantity.** `sched.switches` counts switches out through
`perf_event_open`; `sched.offcpu` counts blocks through a raw tracepoint. A
task leaves the cpu for two reasons — it was waiting, or the scheduler took the
cpu away — and only the tracepoint can tell them apart, because only it sees
the outgoing task's state. So perf always counts at least as many, never fewer,
and the difference is preemption.

The catalog says this as a caveat. The scenario makes it a number. On an idle
machine the two agree to half a percent (2011 against 2002). Measured inside
this test suite, with fourteen other tests running:

```
perf saw 2196 switches and the tracepoint saw 1972 blocks, 10.2 % apart
```

A tenth of the answer, and a tenth that a reader of the switch count alone
would have taken for blocking.

**Every unimplemented backend, against the refusal.**
`examples/sched-forced-backends.yml` pins the five backends that sit in a chain
whose other end is implemented — where a reader is most likely to reach for
`--backend force:` and most likely to be misled. Each must exit 3, "this build
does not implement it", and not exit 1, "this machine cannot serve it": every
requirement of every one of them probes *available* here, which
`omn list sched.offcpu` shows. It also has to say the resolver was bypassed,
because a pinned backend is not a measurement anyone should quote.

### Where did a program block, not how long? — `examples/sched-blocking-stack.yml`

`sched.offcpu --emit stack` walks the task's user stack in the kernel at the
`sched_switch` where it leaves the cpu — the only place that stack exists,
since by the time the task is scheduled back in what is under it is the
scheduler. The stacks are interned in a BPF map, so four bytes travel with each
block and a call site is walked once however often it blocks.

The workload sleeps from one known call, so the leaf frame is known too. Ground
truth is the name `clock_nanosleep`, and the check is that it appears:

```
$ python3 -c 'import time
for _ in range(20): time.sleep(0.05)' &
$ omn sched.offcpu --pid $! --emit stack -d 400ms
  stack
    750.78ms  1 events  _start;__libc_start_main;libc.so.6+0x27781;Py_BytesMain;
              Py_RunMain;libpython3.14.so.1.0+0x2fe4fd;…;clock_nanosleep
```

The same request against a program `omn` launches itself comes back with twelve
frames and not one name. That is a defect, and so is what the request costs;
both are below.

### Does the tool say what it did not see? — `examples/sched-wakeup-loss.yml`

`sched.wakeup` streams one 48-byte record per wakeup through a 256 KiB BPF
ring, drained every 10 ms: about 4 700 records a drain, or 470 000 wakeups a
second before the ring is the limit. The workload beats that with two pipe
ping-pongs in one process — threads rather than processes, because a process
target keeps the wakeups its *thread group* takes part in and `os.fork` would
put the peer in another one.

Two pairs, twenty thousand round trips each, so about 80 000 wakeups. Three
consecutive runs:

```
  63337 events over 326.85ms   LOSS: 16863 dropped, 0 failed reads, 8 late ticks
  71906 events over 336.46ms   LOSS:  7811 dropped, 0 failed reads, 2 late ticks
  63552 events over 267.71ms   LOSS: 16370 dropped, 0 failed reads, 0 late ticks
    11x the wakeup ring was full: wakeups happened faster than they were drained
```

The first run above is 21 % short of the 80 000 the workload performed, and
only the loss line says so. A collector that lost that much and stayed quiet
would have produced a wrong number that looked right, and by the time anyone
noticed the run would be gone.

The test does not assert that shortfall, and the reason is worth stating
because it is the limit of this scenario. Run inside the suite, with fourteen
other tests on the machine, the same workload sometimes reports *more* than
80 000 events while still dropping thousands: a contended interpreter lock adds
wakeups of its own, so two per round trip stops being the rate. The shortfall
is real and reproducible standing alone, and its size is the machine's to
decide, so the number is here and the invariant is in the test — the loss is
counted, it is named, and the events plus the drops still account for the round
trips.

Both of this family's loss kinds show up here. `dropped` is the ring; `late
ticks` is the drain taking longer than the 10 ms it is given, which is the same
pressure seen from the reader's side. Each is reported with a sentence, not
just a count.

If a machine drains faster than two ping-pong pairs can fill — a real
possibility on hardware quicker than this one — the test says so in those words
and stops, because that is a machine that cannot serve the scenario rather than
a scenario that passed.

Runtime: 0.4 s.

### Is the reason the right reason? — three files, three exit codes

| File | Exit | What it must say |
|---|---|---|
| `examples/sched-refusals.yml.rejected` | 2 | four requests that cannot mean anything, each named |
| `examples/sched-not-implemented.yml` | 3 | a source this build describes and does not collect |
| `examples/sched-forced-backends.yml` | 3 | a backend this build does not have, pinned on purpose |

The rejections are checked for the reason *and* for what the message goes on to
offer, because a refusal that sends a reader to the wrong flag costs more than
no message at all:

```
error: `sched.wakeup` cannot emit `hist`
  hint: this source emits raw, topk, timeline, summary
error: `sched.offcpu` cannot be observed in `count` mode
  hint: this source supports trace
error: `sched.offcpu` has no field `cpu` to filter on
  hint: `sched.offcpu` exposes comm, pid, state
error: `sched.offcpu` cannot be measured per iface: `sched.offcpu` is not a network interface source
  hint: this source accepts launch, pid, tid, cgroup, container, system
```

The exit-3 message is checked for one sentence in particular — *"The request
itself is valid"* — because that is what stops a reader looking for a kernel
option that does not exist.

**Why the exit-2 file is not called `.yml`.** Every session in `examples/` that
ends in a format's extension is required to validate:
`tests/cli.rs::the_shipped_examples_validate` walks the directory rather than
reading a list, so that no example can ship broken by being left off one. A
session whose entire point is to be rejected cannot satisfy that and should not
have to — it is not broken, it is the class D scenario. Renaming it out of the
walk is the smallest way to hold both rules, and `--input-format yaml` is what a
file with no recognised extension needs:

```console
$ omn -f examples/sched-refusals.yml.rejected --input-format yaml ; echo $?
2
```

The better fix is two lines in that shared test — skip a file whose name says it
is meant to be refused — and it belongs to whoever owns `tests/cli.rs`. It is
the one thing this stream would have changed outside its own files.

**Exit 1 has no honest example in this family.** Every backend of every `sched`
source probes available on this machine, so there is nothing here that the
machine cannot serve. What *does* produce exit 1 is
`omn sched.switches --mode count`, and that is a gap in the build wearing a
machine gap's exit code — below.

### Who unblocks whom? — `examples/sched-wakeup.yml`

The measurement no cpu profiler can stand in for. A blocked thread burns no
cycles and appears nowhere in a profile of the ones that do, so a profile of a
program that spends its life waiting is a profile of the waiting it did not do.

The workload runs five children and waits for each. A child exiting is what
releases the shell, and no two children are the same task, so the run must
contain five wakeups of the shell by five *distinct* wakers. It does:

```
wakee 3627861 (sh)  waker 3627862
wakee 3627861 (sh)  waker 3627863
wakee 3627861 (sh)  waker 3627864
wakee 3627861 (sh)  waker 3627865
wakee 3627861 (sh)  waker 3627866
```

Five blocks, five wakers, named — and the five tids are consecutive, which is
the five `sleep` children in the order they were forked.

The same run also shows what the source's caveat warns about. Four other
wakeups appear, of `migration/0`, `migration/11` and two `tailscaled` threads,
all credited to the shell. The shell woke none of them: those are timer and
network softirqs running on its cpu, in its context. **Read a waker as
evidence, not as proof.** `examples/sched-round-trips.yml` is where the wakers
are unambiguous, because both ends of every hand-off are the workload's own.

### How many times was a task moved? — `examples/sched-migrations.yml`

Migrations are usually read as a symptom, and a symptom is hard to check
because nothing chose how many there would be. So the workload chooses: it
rewrites its own cpu affinity two hundred times, to a different core each time,
and a task whose affinity no longer admits the core it is on is moved at once.

Measured: 201 and 204 for 200 rewrites, 307 for 300 and 604 for 600. The few
extra are the shell being placed at startup, which the kernel counts as a
migration too.

### Where did the time go — on cpu, or waiting? — `examples/sched-half-blocked.yml`

The workload phase two's P2 asks phase one to leave behind. A driver and a
responder pass a byte back and forth and **both spin the same number of shell
iterations before answering**, so the driver is on the cpu for its own spin and
blocked for the responder's. The two halves are equal because the same loop
bound is written twice — not because anything measured them.

```
blocked 182.5 ms across 605 blocks      (sched.offcpu)
on cpu  210.0 ms across 42 polls        (cpu.oncpu_time)
1244 wakeups                            (sched.wakeup, 600 round trips)
```

46 % of the accounted time is time the program was not running, which is the
shape a cpu profile alone gets wrong by exactly half.

---

## Workloads

What each one guarantees, for whoever reuses them.

**The pipe ping-pong** — `sched-round-trips.yml`, `sched-sweep.yml`.
A driver and a responder exchange one byte through two fifos, *N* times, in
`/bin/sh`. Guarantees, for the driver: *N* switches out, *N* blocks, 2*N*
wakeups, *N* run queue waits, all to within a couple of percent on an idle
machine. Blocks are microseconds, so the program is almost entirely on the cpu
in wall-clock terms — it is a *count* workload, not a *duration* one. Costs
about 120 ms for *N* = 2000.

**The held lock** — `sched-futex-known-wait.yml`.
Two Python threads and one lock. The main thread holds it before the waiter
exists and releases it after a chosen delay, so the waiter's wait is that delay
minus its own thread creation, and it is the only wait longer than a
millisecond anywhere in the process. Guarantees: exactly one long futex wait,
of the chosen length, on a thread that is not the main one. Costs the delay
plus about 100 ms. Needs `python3`.

**The shared inode** — `sched-lock-shared-file.yml`.
Twelve copies of `/bin/sh`, renamed `omnlockprobe` so a machine-wide run can be
narrowed to them by `comm`, appending to one file or to twelve. Guarantees:
thousands of waits on one inode's rwsem in the first case and none in the
second. Does *not* guarantee anything about the total number of lock waits —
see the finding above, where the uncontended run has two hundred times more.
Two scenarios using it must not run at once.

**The half-blocked ping-pong** — `sched-half-blocked.yml`.
The same shape with both ends spinning a fixed number of iterations before
answering. Guarantees that the driver's on-cpu time and its blocked time are
equal by construction. 600 round trips is about 400 ms. This is the one P2
wants.

**The threaded ping-pong** — `sched-wakeup-loss.yml`.
Several pipe ping-pongs as thread pairs inside one `python3` process, so every
wakeup is inside one thread group and a process target keeps all of them.
Guarantees a wakeup rate above what a 256 KiB ring drained every 10 ms can
hold. Two pairs at twenty thousand round trips is about 0.35 s and loses
between 8 000 and 17 000 wakeups.

**The affinity walk** — `sched-migrations.yml`.
A shell that rewrites its own affinity *N* times with `taskset -p`, to a
different core each time. Guarantees *N* migrations. About 0.4 s for *N* = 200,
most of it the two hundred `taskset` forks.

**The known sleep** — `sched-blocking-stack.yml`.
`python3` sleeping a fixed time. Guarantees a block of that duration whose leaf
frame is `clock_nanosleep`. The only duration ground truth in the family:
`omn sched.offcpu --emit raw -- /usr/bin/sleep 0.3` reports `total_ns=300053029`
against 300 ms asked for, and the 53 µs is the scheduler's timer slack, not
error in the measurement.

**Dependencies.** The ping-pongs and the affinity walk are `/bin/sh` and
`taskset`; the loss workload and the stack workload need `python3`, because no
shell reaches the wakeup rate that fills the ring and because a shell has no
threads. Where a program is missing the test says which, by name, and does not
pass.

---

## What cannot be verified here

**The origin of a migration.** `sched.migrations` puts the arrival core on the
event twice, as `cpu` and as `dest_cpu`, deliberately: "the cpu" of a migration
is ambiguous and this build resolves it one way rather than shipping two
numbers a filter could confuse. The consequence is that "which core is shedding
work" has no answer in this family, and no scenario here can invent one.

**A distribution of blocks.** `sched.offcpu` aggregates in the kernel, and what
comes out is a total and a count per `(task, state, stack)` per drain. The
individual blocks do not survive that, so no scenario can check the *shape* of
the blocked-time distribution — only its total and its count. `hist` is
declared anyway and renders something, which is a defect rather than a limit;
see below. `sched.runqlat` does bucket in the kernel and its histogram is real.

**Voluntary against involuntary switches, from `pmu:sw`.** The perf backend
counts both as one and cannot be made to separate them; the scenario measures
the gap between it and the tracepoint instead, which is the same information
arrived at from outside.

**Anything about `sched.mutex`.** One of the family's ten sources has no
collector, and its scenario is the refusal. ~~Five~~ — `sched.lock`,
`sched.futex`, `sched.irq` and `sched.numa_balance` landed in the sixth wave
and have scenarios of their own below.

**`sched.numa_balance`, on this machine, at all.** It has a collector and it
can never fire here: one NUMA node, so the balancer has nowhere to move
anything to and `sched_stick_numa` is never raised. That is not a skip in
`tests/scenario_sched.rs` — what the test asserts is that the *tool says which
of the two empty series it is*, with the node count in it, because an empty
series from a machine that cannot produce one and an empty series from a
balancer with nothing to do look identical and only the second is about the
workload.

**Soft interrupts.** `sched.irq` is the hard half only; see
`docs/families/sched.md` for why, and for the catalog line that would close it.

**A `cgroup` target, for any source in this family.** Six entries declare
`cgroup: bpf_only` and six collectors refuse it by name. No scenario here can
check a narrowing that does not exist.

**The machine-wide stack scenario.** It is in `examples/` in spirit and not in
fact: `omn sched.offcpu --system --emit stack -d 200ms` does not finish, so
there is nothing to check. The reason is a defect and is below.

**Kernel-side blocking stacks.** `bpf:raw_tp` asks for `BPF_F_USER_STACK`, and
the catalog states the degradation: the kernel half of any block is the
scheduler, and what the task was waiting for is the state, not the frames. So
"blocked in the block layer" is not something a scenario here can assert.

---

## The sixth wave's scenarios

Four sources, and their ground truths are deliberately three different kinds:
a difference the workload constructed, a delay the workload injected, and a
counter the kernel keeps for its own reasons. One source has no ground truth
available on this machine, and the scenario for it is about saying so.

### What does sharing one file cost? — `examples/sched-lock-shared-file.yml`

Twelve processes append to one file; then the same twelve append to twelve
files. A write to a regular file holds that file's inode lock — an rwsem, taken
for writing — for the length of the write, so the first run serialises twelve
writers on one lock and the second never touches it. Same program, same number
of processes, same number of writes of the same size. The only difference
between the two runs is whether the object is shared, which is what makes this
ground truth rather than observation.

```
                       write (rwsem)              spin
  one shared file    2233 waits, 5.82 s     1602 waits, 16.3 ms
  twelve own files      0 waits                838505 waits, 18.4 s
```

**The finding is in the second column, and it is the most useful thing this
wave measured.** The *contended* run has two hundred times fewer lock waits in
total. Both numbers are right and the reason is arithmetic rather than
mysterious: serialising twelve writers on one inode makes them slow, slow
processes make fewer syscalls, and it is the syscalls that reach the
filesystem's own spinlocks. The private run is two hundred times faster and
pays for it in two hundred times more brief contention somewhere else.

So a count of lock waits is a throughput measurement wearing a contention
measurement's name, and the two go opposite ways. A reader ranking two runs by
how many lock waits each had will order them backwards, and nothing in a count
says so. What separates the runs is *which* lock was contended and for how
long — 5.8 seconds of aggregate waiting on one inode's rwsem at a mean of
2.6 ms a wait, against none at all — and that is why the lock's flags are part
of the row key rather than a field carrying their last value.

`tests/scenario_sched.rs` asserts both halves: the rwsem separation, and the
trap. The second is written as an assertion rather than a comment on purpose —
if the counts ever stop going opposite ways on this machine, this document
needs the new numbers rather than the test needing a smaller claim.

Two smaller things this scenario established. The workload's processes run
under a name of their own — a copy of `/bin/sh` called `omnlockprobe` — because
the source has no per-process target that sees twelve separate processes, so
the session is machine-wide and narrowed by `comm`. And the two scenarios that
use that workload hold a lock against each other: run concurrently, each
measures the other's writers, and it was the attribution assertion that caught
it rather than a guess that it might happen.

### A lock held for a known time — `examples/sched-futex-known-wait.yml`

The main thread takes a lock **before the second thread exists**, starts it,
sleeps 500 ms and releases. The second thread's wait is 500 ms minus however
long it took to be created, and nothing else in the program waits for anything.

```
one wait, on one thread, of 499.7 ms, in the [268.4 ms, 536.9 ms) bucket
```

The check is against the injected delay in two ways, because they fail
differently. The **exact total** is a fact about the clock: `total_ns` is kept
beside the count precisely because a log2 bucket is a factor of two wide and a
sum built from its bounds would inherit that. The **bucket containment** —
`low <= total < 2*low`, asserted on every event carrying a single wait — is a
fact about the histogram's layout: it is built in the kernel and re-bucketed by
the emit layer, and off by one there is a factor of two in every number this
source reports. The second holds whatever the machine was doing, which is why
it is written against the value rather than against a bucket the workload was
expected to land in.

**This workload is on its second design, and the first one is the lesson.** It
had the two threads take and release the lock in turns, three times, with the
waiter starting 50 ms late: one wait of 200 ms and two of 250 ms. It measured
beautifully on an idle machine and lost one of the three under the full suite —
if the waiter is late enough, the holder has already released and there is
nothing to wait for. The property was real and the *workload did not guarantee
it*, which is the difference this whole document is about, and it is finding
11's shape in a scenario of this wave's own making. The version above cannot
lose a wait: taking the lock before the waiter exists removes the race rather
than making it unlikely, and the assertion is now that there is exactly one
long wait in the entire process.

The workload is Python because that is what is on the machine, and the session
carries the program in full rather than pointing at a file: a session nobody
can read is a session nobody can check. A machine without `python3` skips with
that sentence rather than passing quietly.

What this scenario cannot establish is the *absence* of waits. An uncontended
`pthread_mutex_lock` never reaches the kernel — it is an atomic in the
program's own memory — so a program with no contention measures as nothing
here, and nothing is also what a broken collector measures.

### Every interrupt this run timed is one the kernel also counted — `examples/sched-irq-against-procfs.yml`

`/proc/interrupts` is the ground truth, and it is a good one for the reason
`docs/contributing.md` asks for: the kernel maintains those counters itself, in
a different place and for a different reason, so they are not this tool
checking its own arithmetic. What this source adds is the only thing that file
does not have — how long each handler ran.

```
 irq  tool  procfs  name
  70   321     321  nvme0q2
  69   154     154  nvme0q1
  71     5       5  nvme0q3
  84   387     439  iwlwifi:default_queue
  66   965    1098  nvidia
```

The check is an **inequality**, and deliberately. The tool attaches after the
first read of the file and detaches before the second, so what it counted is
strictly inside what the file counted, always and on any machine. An equality
would be an assertion about how quiet the machine happened to be at the edges
of the window, which is the shape findings 11 and 35 are about. The exact lines
above are the ones that were quiet there; the others differ by what arrived
while the tool was starting and stopping.

One line is excluded and the reason is not incidental: on a shared interrupt
line the tracepoint fires once per handler and `/proc/interrupts` counts once
per interrupt, so a line with two handlers legitimately gives the tool twice
the file's number. Lines whose action list has a comma are skipped.

The inequality alone would be satisfied by a collector reporting one interrupt
in a hundred, so the busiest comparable line is also checked to be at least
half of what the kernel saw. And the handler names have to be the same string
the file uses, or `--filter name` selects something other than what a reader
looked up.

### The source that cannot fire here — `examples/sched-numa-balance.yml`

One NUMA node:

```console
$ ls -d /sys/devices/system/node/node*
/sys/devices/system/node/node0
```

so `sched_stick_numa` is never raised, and there is no measurement to check.
The scenario is therefore about the tool's honesty rather than its arithmetic,
and it is a test rather than a skip: on a machine with one node the run must
report no decisions **and** say that the emptiness is the machine's; on a
machine with more, it must *not* say that. The claim only holds one way, so it
is asserted one way.

---

## The queue wait inside a block

Added by the eighth wave's coordinator pass, closing the compute crossing's
finding 1. Two scenarios, and the first of them is the one that matters:

| Scenario | What it checks | Where the number comes from |
|---|---|---|
| `a_block_says_how_much_of_it_was_waiting_for_a_cpu` | `blocked_ns + queued_ns == total_ns`, the wait is inside the block, and under contention it is neither nothing nor everything | two spinners pinned to one core with `taskset`, so the sleeper's wakeups have to queue |
| `a_block_whose_wakeup_was_not_seen_says_so_rather_than_reporting_no_wait` | a `queued_ns` of zero that means *unknown* is said in words | a machine-wide run, where some thread is always already asleep when the programs attach |

The arithmetic is asserted and the threshold is not: how long a queue wait is
belongs to the machine, and what belongs to the tool is whether it says so and
whether its three numbers are consistent. Where this machine wakes every sleep
onto a free cpu the contention half skips with the numbers, which is the honest
form of "the property could not be created here".

The cross-check against an independent counter is in `tests/cross_compute.rs`,
because that is where the workload reports its own `schedstat`: **93.6 ms of
queue wait reported against the kernel's 96.5 ms** for the same loop, under 30
microseconds per wakeup apart.

## Defects found

Ten from the earlier waves, in the order they cost a reader most, **three of
them now fixed**: 2, 3 and 4, in the fifth wave, by the stream that owns stacks and the two families
that produce them. Each is marked below with what changed and the scenario that
now asserts it, and the reproduction is left in place because that is what the
assertion is written against. `docs/fixes/stacks.md` is that stream's log, and
it also says what became of 10.

None is worked around: each session below is in `examples/` and runs. The last is not in the tool at all — it is a
test of this family that fails about one run in five, and it was found by
running `make check` often enough to notice.

### 1. `sched.offcpu --emit hist` renders a distribution that is not one — **half fixed**

Still true as written. What changed in the fifth wave is that the emit layer can
now be *told*, and the diagnosis `BRIEF.md` carried is ruled out; both are at the
end of this entry.

The workload performed 19 951 blocks of a few microseconds each. The histogram
shows nine samples with a mean of 15.55 ms and a maximum of 29.01 ms — buckets
no block ever occupied. The rows being bucketed are *drains*, not blocks.

```
$ omn sched.offcpu --emit hist,summary,raw -- /bin/sh -c '<ping-pong, 20000 round trips>'

  hist (log2)
    task:3693029  n=9  min 62.02us  mean 15.55ms  max 29.01ms
      [32768ns, 65536ns)                    1  |######                          |
      [65536ns, 131072ns)                   1  |######                          |
      [524288ns, 1048576ns)                 2  |############                    |
      [16777216ns, 33554432ns)              5  |################################|
      (the empty buckets between these four are elided)

  summary
    task:3693029 n=9     min 62.02us  mean 15.55ms  max 29.01ms  last 27.30ms

  (raw carries blocks= on every row; they add up to 19951 across those 9 rows)
```

The information is not lost — `blocks` is on every raw event — but `hist` and
`summary` throw it away and present the drain windows as if they were the
measurements. The run's only `degraded:` line is about stacks; nothing says the
distribution is of something else.

`sched.runqlat` is the counter-example in the same family, and it is what makes
this a defect rather than a missing feature. It buckets in the kernel and says
so with every run:

```
degraded: 2045 waits are reported as 103 rows of a kernel-side log2 histogram,
each carrying the waits it stands for; a bucket is a factor of two wide, so
every number derived from `wait_ns` is bucketed and `total_ns` is the exact sum
```

The fix is the one `docs/families/sched.md` already names: put a log2 bucket in
the key of the map the program keeps, as `runqlat` does, and the histogram falls
out of the drain. Until then `hist` on this source should carry a degradation
of its own or be refused, because a reader has no way to tell the two
histograms apart.

**What landed, and what did not.** `BRIEF.md` routed this to the emit layer with
the guess that the cause is "the one `emit::WEIGHT_ATTR` was added for, in a
source that does not yet carry it". It is not, and the arithmetic says so:
`WEIGHT_ATTR` means *this value occurred N times*, which is true of a
`sched.runqlat` row — `(task, bucket) -> count`, every wait behind it inside
that bucket — and false here, where `total_ns` is the **sum** of `blocks`
blocks. Weighing this row by its block count would report four hundred blocks of
about twenty microseconds at the value of their sum, in a bucket none of them
was ever in: a worse answer than the one above, not a better one.
`emit::tests::a_row_that_is_a_total_over_its_measurements_says_what_it_is`
carries that arithmetic so it is not re-derived.

What the emit layer gained instead is the vocabulary it was missing:
`emit::TOTAL_OF_ATTR`, a collector saying *this value is the sum over N
measurements whose individual values do not reach here*. `raw`, `rate`,
`timeline` and `topk` are unaffected by it, since a total is exactly what they
add up or plot; `hist` and `summary` are the two that claim to describe
individual measurements, and `emit::aggregate` puts a degradation beside them
naming both counts — the degradation this entry asks for.

It is inert until a collector carries the attribute, and
`src/collect/sched/offcpu.rs` belongs to another stream of this wave, so the
line is written out in `docs/open-findings.md` under "Needs from another
stream". Nothing in the tool has changed yet, which is why this defect is half
fixed and not fixed, and why there is no assertion on it here. The real fix is
still the kernel-side bucket; when it lands, the row carries `WEIGHT_ATTR`
instead and `TOTAL_OF_ATTR` comes off.

### 2. A blocking stack of a *launched* program comes back with no names

**Fixed** (wave five), and the diagnosis below was wrong about the cause. The
child *is* readable: `collect/launch.rs` restores its dumpability before it
execs, and `omn` — its parent — reads its mappings without trouble. What the
`ls` and `head` below show is a *third* process being refused, which is Yama's
`ptrace_scope`, not the child being non-dumpable.

The real cause is timing, and the window `open_target_symbols` documented does
not exist. The runner reads every collector once before it sleeps, so the first
tick is not 100 ms after the launch: it is the instant after the gate opens,
with the child inside `execve`, its old image torn down and its new one not yet
mapped. Instrumented, the first read of `/proc/<pid>/maps` returned two lines —
`[vsyscall]` and nothing else. The one-shot re-read that `Symbolizer::region_of`
does when an address falls outside its snapshot then ran at the *other* wrong
moment: for a program that blocks once and exits, the drain carrying that block
happens in `stop`, after the program is gone, so the re-read returned nothing —
and *adopted* it, replacing what little it had with none.

Two changes, both in the shape the trap keeps taking:

- **A symbolizer never adopts an empty read.** An empty `/proc/<pid>/maps` is
  not a process with no mappings; it is a process mid-`execve` or a process that
  has exited. `Symbolizer::refresh` keeps what it holds, and `region_of`'s
  re-read does the same and retries a bounded number of times rather than
  spending its one attempt on a read that answered nothing.
- **There is no right moment, so both collectors take them all.**
  `sched.offcpu` refreshes the target's mappings on every tick, and
  `cpu.profile` does the same before each drain. The snapshot a stack is
  resolved against is then the most recent one taken while the target was
  alive, which is the best answer that exists.

The launched run now reads:

```
  stack
    300.05ms  1 events  _start;__libc_start_main;libc.so.6+0x27781;Py_BytesMain;
                        Py_RunMain;…;clock_nanosleep
```

Pinned by `a_blocking_stack_names_the_call_that_blocked`, which now runs both
targets. The launched half uses a program that blocks **once** and then exits,
because that is the shape that fails: a workload sleeping every fifty
milliseconds for a second symbolizes even on the old build, since some drain
catches it alive. It asserts the leaf by name and that most of the twelve
frames carry one, so a snapshot caught mid-`execve` resolving the odd address
does not pass.

Same program, same build, same request, two targets:

```
$ omn sched.offcpu --emit stack -- python3 -c 'import time
time.sleep(0.3)'
  stack
    300.05ms  1 events  0x5630f5a62045;0x7f34174278b9;0x7f3417427781;0x7f3417aa3efc;
                        0x7f3417aaa232;0x7f3417afe4fd;…;0x7f34174e4aab

$ python3 -c 'import time
for _ in range(20): time.sleep(0.05)' &
$ omn sched.offcpu --pid $! --emit stack -d 400ms
  stack
    750.78ms  1 events  _start;__libc_start_main;libc.so.6+0x27781;Py_BytesMain;
                        Py_RunMain;…;clock_nanosleep
```

Twelve frames either way, so the kernel walked the stack and the map held it.
What fails is symbolization, and only for a program this build launched itself.

The cause is visible from outside. While `omn` runs a launched child, that
child's mappings cannot be read — by anything, including `omn`:

```
$ ls -l /proc/3510427/maps
-r--r--r-- 1 moha moha 0 Sep 19 17:07 /proc/3510427/maps
$ head -2 /proc/3510427/maps
head: cannot open '/proc/3510427/maps' for reading: Permission denied
```

Owned by the user, mode 444, and refused: the signature of a non-dumpable
process. That is finding 14 of `docs/open-findings.md` seen from the other
side. `main.rs` restores `omn`'s own dumpability because file capabilities cost
it; nothing restores the child's, and a child that `execve`s away from an
inherited capability set is made non-dumpable at that `execve`.

`sched.offcpu` already anticipates the *timing* trap — `open_target_symbols`
builds the symbolizer at the first tick, documented as the only window in which
a launched program has both execed and not yet exited, and it is correctly
called from `sample`. The window is right; the permission is not.

`cpu.profile --emit stack` against a launched `python3` does symbolize, so
whatever it does to learn the target's mappings is the thing `sched.offcpu`
should do too. That last observation was the useful half: what `cpu.profile`
did differently was drain every 10 ms, so its one-shot re-read almost always
landed while the target was alive. It was luck rather than method, and it is
now method in both.

### 3. `--emit stack` costs about 0.65 s per process, and machine-wide never finishes

**Fixed** (wave five), by the same change that closes defect 7 of
`docs/scenarios/cpu.md`: `/proc/kallsyms` is read at most once per run, into a
process-wide `OnceLock`, and only when a kernel frame is actually symbolized.
This source asks the kernel for `BPF_F_USER_STACK` and never looks a kernel
frame up, so it now never reads the file at all. Measured again:

```
omn sched.offcpu --emit raw   -- /usr/bin/sleep 0.2     0.445 s   (was 0.456 s)
omn sched.offcpu --emit stack -- /usr/bin/sleep 0.2     0.455 s   (was 1.096 s)
omn sched.offcpu --system --emit stack -d 200ms         2.3 s     (did not finish)
```

Pinned by `asking_for_stacks_costs_the_stacks_and_not_the_kernels_symbol_table`,
whose reference is the same request without stacks over the same workload — so
what is asserted is the difference between the two, not a wall clock this
machine happens to produce — and which requires the machine-wide run to return
at all.

```
omn sched.offcpu --emit raw   -- /usr/bin/sleep 0.2     0.456 s
omn sched.offcpu --emit stack -- /usr/bin/sleep 0.2     1.096 s
omn sched.offcpu --system --emit stack -d 200ms         still running after 2 minutes
```

The 300 ms run reports it in its own words: *"spent 908.8 ms of cpu against the
target's 10.2 ms (8946.58 %)"*.

`Symbolizer::for_process` calls `KernelSymbols::read()`, which reads and parses
`/proc/kallsyms` — 395 318 lines on this machine — and `sched.offcpu` builds
one symbolizer per process it sees. Machine-wide that is one full parse per
process that blocked, which is why a 200 ms window does not return.

The kernel table is then never consulted: the collector asks the kernel for
`BPF_F_USER_STACK` and the code says so — *"User frames only: putting them
through kernel symbols would name them after whatever function lives at that
offset"*. So the whole cost is paid for a table that is deliberately unused.

`offcpu.rs` already carries a comment about a related measurement — *"`--system
--duration 1s --emit summary` did not finish in ninety seconds, and finishes in
1.2 with this"* — which fixed the case where stacks were resolved and thrown
away. The case where they are actually asked for is still there.

### 4. `sched.migrations --mode count` is `--mode trace` under another name

**Fixed** (wave five). `count` is now what `Collector::stop` says it is: the
single measurement of one window. The drains still happen, because the kernel's
map has to be emptied, but what they produce is accumulated per (task, cpu) and
reported once at the end, over the window the request asked for. `trace` is
unchanged — one row per pair per drain, each carrying the interval it covers.

```console
$ omn sched.migrations --mode trace --emit raw --system -d 400ms
  … window_ns=250066670 …          # one row per pair per drain
$ omn sched.migrations --mode count --emit raw --system -d 400ms
  degraded: `count` reports one total per (task, cpu) over the whole window, so
            there is no series here and `rate` over it is the window's mean;
            `--mode trace` is the one that reports a count per drain
  … window_ns=500701149 …          # one row per pair, one window
```

Pinned by `counting_migrations_is_one_window_and_tracing_them_is_a_series`. Its
ground truth is the one this family already has — a task whose affinity no
longer admits the core it is on is moved at once, and the loop rewrites it two
hundred times — so both modes must report the *same total* in two different
shapes. It drives the two modes from the command line rather than from the
session, because `--mode` beside `-f` is accepted and discarded (cpu 5), and a
scenario that asked the session for `count` would be handed `trace` and would
check nothing.

Both modes are declared, both are accepted, and they produce the same output.
Diffing the two sections of one sweep run leaves only the `topk` block that
`count` refuses:

```
$ diff <(omn sched.migrations --mode trace --emit raw,summary …) \
       <(omn sched.migrations --mode count --emit raw,summary …)
(no difference in the events, the windows, or the series)
```

The collector takes the mode, checks it is one of the two, and discards it:
`Prepare(Mode)` refuses anything sampled and then never reads the value again.
Meanwhile `Collector::stop`'s own doc comment describes what `count` is meant
to be — *"the single measurement of a `count` window, which has no series and
whose closing read **is** the result"* — and `sched.switches` honours that by
refusing `count` outright rather than faking it.

Two sources in one family answer the same axis in two ways, and one of them
pretends. A mode that changes nothing is worse than a mode that is refused,
because the refusal is visible.

### 5. `count` mode refuses `topk` on a run that has series to rank

```
$ omn sched.migrations --mode count --emit topk:3 --system -d 100ms
error: mode `count` cannot produce `topk`
  hint: in `count` mode the valid emits are rate, summary, raw
```

The reasoning is that a counted window is one number. It is not, here: the same
counted run renders `summary` as twenty-odd per-task series, and `topk` is
exactly the emit for ranking them. The catalog declares `topk` on this source
and declares `count` as a mode, and the pair is refused by a rule that the run
itself contradicts.

Stream B of the fifth wave looked at this and left `catalog/sched.yaml` alone:
the entry is not the thing that is wrong. `sched.migrations` declares `trace`
too, so its `topk` is reachable in principle and the entry promises nothing no
build could keep; deleting the mode or the emit would have removed the
capability rather than the defect. What refuses it is one arm of
`ModeKind::valid_emits` in `src/request.rs`, which is outside every stream's
paths in this wave and is written out under "Needs from another stream" in
`docs/open-findings.md`. The same line closes net 1 and the `--emit topk` half
of io 1.

### 6. An under-specified request exits 1 where the same kind exits 2

> **Fixed** (stream A). A `ParseError` from `cli.rs` reaches exit 2, like the
> validator's answer to the same kind of mistake; `sched.irq` and
> `sched.numa_balance` answer the same way. Asserted by
> `an_under_specified_request_is_rejected_rather_than_failed`.

Two requests, both missing one required axis, both unanswerable for the same
reason:

```
$ omn sched.switches --emit summary -- /bin/true
error: `sched.switches` needs --mode: one of count, trace
exit 1

$ omn sched.switches --mode trace -- /bin/true
error: `sched.switches` was requested without an emit
  hint: add --emit with one of rate, summary, topk, raw
exit 2
```

`BRIEF.md`'s table has one meaning for each code: 2 is "the request cannot mean
anything" and 1 is "this machine cannot serve it". A missing `--mode` is the
first and reports the second, so a script that branches on the exit code sends
its reader to look at the machine. The missing emit is raised by the validator
and reaches `EXIT_REJECTED`; the missing mode is a `ParseError` from `cli.rs`
and falls through `main`'s generic `ExitCode::FAILURE`.

The same applies to every source with two modes: `sched.irq` and
`sched.numa_balance` answer the same way.

### 7. `--keys` is accepted, recorded, and not used

`sched.switches` declares `keys: [task, cgroup, cpu]`. Asking for `cpu` gets
`task`, with nothing said:

```
$ omn sched.switches --mode trace --emit summary -k cpu -- /bin/sh -c 'sleep 0.02'
  summary
    task:3694503 n=3  min 1 count  mean 1 count  max 1 count  last 1 count
```

The key is in the dumped request — `"keys": ["cpu"]` — so it was parsed and
carried, and `--dump-resolved` writes it into the record kept beside the
numbers. What reads it is nothing: `SeriesKey::of` takes the *first* key the
collector happened to attach, and no `sched` collector consults
`resolved.request.keys` at all. `sched.runqlat` shows the other half of the
same thing — it emits `task:… cpu:…` whether or not `cpu` was asked for.

Keys are one of the five axes the tool advertises. Silently substituting one is
worse than refusing the request, and much worse than saying which key it
actually grouped by.

This is not a `sched` defect — no collector anywhere reads the request's keys —
but it is visible in every session this family ships, so it is recorded here.

### 8. A session reports all its rejections and only its first no-collector

> **Fixed** (stream A). Every request is asked and every refusal is reported
> together, which is the courtesy the validator already extended one layer up.
> Asserted inside
> `pinning_a_backend_this_build_lacks_blames_the_build_and_not_the_machine`.

`examples/sched-refusals.yml.rejected` has four invalid requests and prints four errors.
`examples/sched-forced-backends.yml` has five requests with no collector and
prints one, then stops. Same shape of file, same kind of answer, two behaviours;
a reader fixing a session learns about its problems one run at a time.

### 9. `sched.switches --mode count` blames the machine for a gap in the build

> **Left** (stream A), and it is the one defect of this group that the runner
> cannot settle on its own. `sched.switches` refuses from `Collector::start`,
> against a target that already exists, where a refusal is genuinely ambiguous
> between a mode nobody wrote and a counter that would not open -- the same line
> a real `perf_event_open` failure arrives on. `collect::not_built` exists for
> the collector to say which; the one-line change is in
> `docs/open-findings.md` under "Needs from another stream", in the file stream D
> owns.

```
$ omn sched.switches --mode count --emit summary -- /bin/true
error: cannot start collecting `sched.switches`: `count` mode for `pmu:sw` needs
the perf layer's counting support (see STATE.md); ask for `--mode trace` to get
one event per switch
exit 1
```

The message is exactly right and the exit code is exactly wrong: nothing about
this machine is missing, and exit 3 is the code for "this build does not
implement it". `STATE.md` already lists this under *the smaller ones* — *"an
exit code for 'valid, but this mechanism is not built' — it returns 1, like a
real failure; `NoCollector` returns 3 and this belongs with it"* — and this is
the `sched` instance of it. The test here checks the message and leaves the
code to that work.

### 10. `tests/family_sched.rs` fails about one run in five, and takes a second test with it

**Fixed** by the coordinator, after stream D of wave five explained it. It is a
collision between worktrees, in a file no stream of that wave owned.
`distinctly_named_shell` builds its workload at `std::env::temp_dir()/<name>`,
which is `/tmp/omnrunqshare` — a machine-wide path. Six worktrees of this wave
ran the same suite on this machine, each copying `/bin/sh` there and each
spinning a task that answers to that `comm`, and the filter the test uses is
`comm == 'omnrunqshare'` machine-wide. Demonstrated directly: two processes
started from `/tmp/omnrunqshare` give two tids under exactly that filter.

The name now carries the test process's pid, so a run's workload is its own and
the `--system` filter selects only it. That was not the whole of it: two copies
of this suite also pin their spinners to *the same core*, so "a task alone on a
cpu waits for nothing" was measuring the other copy's three. The test now reads
`/proc/stat` for that core before it starts, and skips with the number when the
core is already busy — the same shape as every other precondition in this
project, and the reason the flake survived one fix.

That accounts for the whole table below — the test alone never failed, and it
failed when the machine was busiest — without needing either of the two causes
that were suspected. The collector is not at fault: `sched.runqlat` reads `comm`
from `task_struct.comm` in the kernel at enqueue, so there is no per-tid cache
to attribute a recycled tid to its previous occupant.

The fix is to make the name unique per run rather than per test, so that one
worktree cannot count another's load — the pid fits inside the fifteen
characters a `comm` keeps:

```rust
let name = format!("{name}{}", std::process::id() % 100_000);
```

It belongs in `tests/family_sched.rs`, which stream D of this wave may not
edit; the request is under "Needs from another stream" in
`docs/open-findings.md`.

Not a defect in the tool, and not this stream's file to fix, but it is what
`make check` does on this machine and the next person to run it should know why.

```
---- tasks_sharing_one_cpu_wait_the_share_the_scheduler_owes_them ----
assertion `left == right` failed: 1 pinned tasks must each appear once
  left: 2
 right: 1

---- the_kernels_buckets_are_the_emit_layers_buckets ----
panicked at tests/family_sched.rs:620: the cpu lock: PoisonError { .. }
```

The first pins **one** cpu-bound spinner to one core, measures `sched.runqlat
--system --filter comm == 'omnrunqshare'` for a second, and finds two tids
carrying that name. What it is measuring is right; how it identifies its own
workload is by a name, machine-wide, and something else answers to it.

What was tried, and what it rules out:

| | |
|---|---|
| that one test alone, 8 runs | never failed |
| its command line driven by hand against one spinner, 12 runs | one tid every time |
| the whole `family_sched` target, 9 runs | failed twice |
| `cargo test --workspace` | failed once |

So it needs the target's other twelve tests running beside it, which rules out
a stale spinner left by a previous invocation — `PinnedSpinners::drop` kills and
reaps — and points at either a second process on this machine answering to that
`comm` while the target runs, or the collector's own per-tid `comm` cache
attributing a recycled tid to the name its previous occupant had. Which of the
two it is, this stream did not establish.

**The second failure is not a second bug.** Both tests take the same
`CONTENDED_CPU` mutex so that only one contends a core at a time, and a panic
while holding it poisons it, so whichever of them runs second fails on the lock
rather than on anything it measured. One flake reads as two, and the second
message points at a line that has nothing to do with the problem.

None of this is caused by the scenarios in this wave: `cargo test` runs test
binaries one at a time — the sum of the per-target times is 31.5 s against a
31.6 s wall clock — so nothing in `tests/scenario_sched.rs` is running while
`tests/family_sched.rs` is.

---

### 11. Pairing kernel lock waits by thread id put every cpu's idle task in one slot — **found and fixed in this wave**

`sched.lock` keys its in-flight map by thread id, which is right for every
thread on the machine except the twenty-four that share the number zero. Every
cpu has an idle task and all of them are pid 0, so a wait opened by one cpu's
idle task was closed by another's, and the duration that came out was the gap
between two unrelated instants. Read as the unsigned number a duration is, a
negative gap is nine billion billion nanoseconds:

```
summary
  task:0 n=94  min 32.00ns  mean 3270699.30min  max 153722867.28min
```

That is 2^63 nanoseconds, bucketed and rendered as calmly as any other value.
It was the first machine-wide run of the collector and it is the reason the
histogram is worth reading at all: nothing about the output says which of those
numbers is real.

Two things were done rather than one. The idle task is excluded from the
pairing, counted, and reported as a degradation with its number — its
contention is real, in interrupt context, and no pairing keyed by thread id can
give a duration for it. And a **guard** was added for a duration that runs
backwards, in every collector this wave landed: the subtraction is checked for
sign before it is bucketed, and a negative one is counted under its own reason
instead. The guard should now be unreachable; it costs one compare, and the
alternative is a histogram nobody can tell is wrong.

### 12. Every loss was reported as "the map is full", on a map that was empty — **found and fixed in this wave**

The first run of `sched.lock` on an idle machine reported twelve dropped waits
with the reason *"more threads were waiting for a lock at once than the
in-flight map holds"*. The map holds sixteen thousand entries and nine were in
use.

The collector had inferred the reason from the only fact it kept — that
`bpf_map_update_elem` returned a negative value — and `-E2BIG` is one of
several things that value can be. What was actually happening was `-EBUSY`: a
hash map takes a per-bucket lock, a lock tracepoint fires inside the code that
takes locks, and the kernel refuses a re-entrant update on the same cpu rather
than deadlocking. On this machine that is the ordinary case, not the
pathological one.

A loss reported under the wrong reason costs more than a bare number would,
because it sends the reader to raise a size that was never the problem. The
programs now branch on the errno the kernel returned and count each reason in
its own slot — full, already-open, busy — and each has a sentence of its own.
`sched.futex`, `sched.irq` and `gpu.job` were written with the same split from
the start.

The shape generalises and is worth stating: **a collector that reduces several
kernel errors to one loss counter is a collector that will name the wrong one.**
The information is there at the moment of the failure and nowhere afterwards.

### 13. Two scenarios that start processes under the same name measure each other

Not a defect in the tool, and recorded because it will happen again. `sched.lock`
has no per-process target that sees twelve separate processes, so its scenarios
run machine-wide and narrow by `comm` — and two of them running in parallel
each measure the other's writers. It was caught by the attribution assertion
(every pid on every event must be one this test started) rather than by a
number looking wrong, which is the argument for writing that assertion at all.
The fix is a lock in `tests/scenario_sched.rs`, held for the length of a
scenario; `tests/scenario_gpu.rs` has the same pattern for the same reason, one
device instead of one machine.

---

## What this family contributes to a crossing

One line, as `BRIEF.md` asks:

**`sched` knows where the time went that `cpu` cannot see, and the key that
carries it is `task`.** A cpu profile accounts for cycles; `sched.offcpu`
accounts for the wall-clock time in which there were none, and
`sched.wakeup` is the only thing that says what ended it. Both are keyed by
task, both are on every session in this directory, and
`examples/sched-half-blocked.yml` is a workload whose two halves are known to
be equal — which is what P2 needs to check that the two families' answers add
up to the program rather than to each other.

Every session here carries `keys: [task]`, including the ones whose own
scenario does not need it, so that re-running the workload for a crossing is
not the same work twice.

---

## Runtimes

`make check` runs these fifteen tests in **1.3 s** wall, in parallel. Every
session, run on its own:

| Session | Wall |
|---|---|
| `sched-blocking-stack.yml` | 1.09 s |
| `sched-half-blocked.yml` | 0.52 s |
| `sched-wakeup-loss.yml` | 0.48 s |
| `sched-migrations.yml` | 0.39 s |
| `sched-round-trips.yml` | 0.27 s |
| `sched-wakeup.yml` | 0.26 s |
| `sched-sweep.yml` | 0.26 s |
| `sched-forced-backends.yml` | 0.15 s |
| `sched-not-implemented.yml` | 0.14 s |

Only one is over a second, and the reason is defect 3: `sched-blocking-stack.yml`
spends about 0.65 s of that parsing `/proc/kallsyms` for a symbol table it then
does not use. Nothing here waits on a timeout, and nothing sleeps for longer
than the measurement needs.
