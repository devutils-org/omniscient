# Crossing — the I/O path

**What did one read cost, from the syscall to the platter?** This build can
answer the second half of that question and not the first, and this document is
mostly about saying which half is which, on every line.

Everything this crossing joins already collects and is already checked against
ground truth: `io.queue` and `io.latency` in `docs/scenarios/io.md` (a known
number of one-page reads found one for one, and the two halves of a request
joined at 99.9 %), `sched.wakeup` in `docs/scenarios/sched.md`. Nothing here is
new mechanism. What is new is the arithmetic that puts the block layer and the
scheduler in one table, a ground truth chosen to check that arithmetic rather
than the sources under it, and the residue the table has to print because the
segments do not sum to the whole.

| Session | What it shows |
|---|---|
| `examples/cross-io-one-read.yml` | 2000 one-page direct reads: both halves of each request joined on the scoped key, and the wakeup that released the reader |
| `examples/cross-io-stale-pointer.yml` | the same streams joined twice, scoped and on the bare pointer, where two readers share request pointers |

The checks are in `tests/cross_io.rs`. Every number below was produced on the
development machine: an NVMe disk with the `none` scheduler, btrfs over one of
its partitions, kernel 7.2.

---

## Where the crossing starts, said first

`ROADMAP.md` phase 4 names ten boundaries for the vertical case. Four of them
are the I/O path, and this is what this build reaches of each:

| # | Boundary | `ROADMAP.md` says | What this build has | Evidence earned here |
|---|---|---|---|---|
| 5 | `read()` → cache miss | fd → `file:dev+ino`, KEY | nothing: `io.syscall` and `io.fs` need `fentry` | **not measured** |
| 6 | FS → block submission | re-read `task_storage` at `block_rq_issue`, IDENTITY | the submitting thread, read from `current` at `block_rq_issue` | **KEY on the task, TEMPORAL on the read** |
| 7 | submit → completion | `blk:rq@ts`, scoped equality, KEY | `io.queue` and `io.latency`, joined on `blk:rq@open_ts` | **KEY** |
| 8 | completion → wakeup | `sched_wakeup` from IRQ, WAKEUP | `sched.wakeup`, paired with the completion by task and order | **WAKEUP** — and not from an IRQ, see below |

**Boundary 5 is absent**, and the crossing says so in its own output rather than
starting silently at the block layer. The decomposition table has a first line
that reads `syscall -> block issue   NOT SEEN ... needs fentry: io.syscall,
io.fs`, and the time that segment took is in the residue, where it is measured,
not in any segment, where it would be invented.

**Boundary 6 is not IDENTITY, and the brief asked for it to be.** `BRIEF.md`
writes "boundary 6 is `IDENTITY`". The mechanism `ROADMAP.md` gives for it is a
request id put into task storage at the request's entry and read back at
`block_rq_issue`, which is what makes it a *propagated* identity: the downstream
event carries something the upstream one put there. The entry is boundary 3,
behind the syscall edge, which is behind `fentry`. What this build has instead is
the thread id `block_rq_issue` reads from `current`, and that is a key both sides
happen to know: it says *which thread* submitted the request, exactly, and says
nothing about *which of that thread's reads* it belongs to. The window decides
that. So every segment below reaches its read through `task:tid` (KEY) inside
the read's own window (TEMPORAL), and the table's header says so. For reads
issued one at a time by one thread the window is unambiguous, which is why the
table is right; it is still the weakest rung, and a thread with two reads in
flight would be where it stops being right.

**Boundary 7 is the one this crossing exists to show**, and it is KEY because
it is a shared typed key. The next sections check it against something the tool
did not compute.

**Boundary 8 on this machine is not a wakeup from the interrupt.** On btrfs a
direct read's completion is delivered by a kworker: the interrupt wakes the
worker, the worker verifies the data's checksums, and the worker wakes the
reader. In every run of the test that counted them, the three most frequent
wakers of the reader after a completion were kworkers — 1980 of 2000 wakeups
between them in one run, 1176 of 2000 in a busier one, the rest not ranked —
and the idle task, which is what an interrupt on an idle cpu looks like, was
never among them. The hop the roadmap drew as one edge is two, and the first of them is credited to whatever
the interrupt interrupted — which is how a chain came back through *this test's
own reader thread* into a different process's reader, and why the engine's
WAKEUP is weaker than KEY and says so.

---

## The ground truth: the reader keeps its own clock

A decomposition checked against the sum of its own parts proves nothing, since
the residue is defined to make it sum. So the reader is instrumented instead:
every read is timestamped on `CLOCK_MONOTONIC` immediately before and after the
`pread`, by the thread that issues it. The tool's timestamps are on the same
clock, which is this project's only clock domain, and that makes a check with
**no slack** possible:

- **Every request the reader's thread submitted must lie inside one of its
  reads.** Issued after its `pread` was called, completed before it returned. A
  collector whose latency were an uptime, whose clock were `CLOCK_REALTIME`, or
  whose join attached a queue half to another life of the pointer fails on the
  first read.
- **No read holds two requests**, because one page with `O_DIRECT` is one
  request.
- **Half the reads carry 300 us of injected work**, spun inside the timed
  window before `pread` is called. That time is above the block layer, where
  nothing in this build looks, so on every such read the request cannot start
  before `called + 300 us`, and the residue is at least 300 us. A decomposition
  that absorbed the unseen segment into a seen one — the rounding the brief
  warns about — fails there.
- **The queue half's issue time is read by a second program.** `io.queue`
  records the moment its request left the software queue, and `io.latency`'s
  issue is derived as completion minus latency. The two programs are loaded by
  two runs. A queue half the scoped key put on the right life of its pointer has
  its issue inside the same read and before the completion; one put on another
  life carries another request's issue, which lies in another read.

`every_read_decomposes_inside_the_readers_own_clock` asserts all four, and
prints the table.

## The decomposition

The table is not something `omn` prints. The engine joins streams and reports
what it matched, at what evidence; it has no way to express *these segments of
this whole, with this residue*, and the request for one is in
`docs/open-findings.md`. So the arithmetic is the test's, over the tool's raw
events and the reader's own clock, and it is printed under `--nocapture`.

Run alone:

```
one read, 2000 of them: task:113142 (omnxa112993), one page with O_DIRECT, every 2nd carrying 300.0us of its own work
  each segment reaches its read through task:tid (KEY) inside the read's own window (TEMPORAL);
  IDENTITY would need a request id propagated from the syscall, which needs fentry
  no loss in any source: every count below is a measurement
  SEGMENT                  EVIDENCE   READS        P50        P99       TOTAL   SHARE
  syscall -> block issue   NOT SEEN       -          -          -           -       -   needs fentry: io.syscall, io.fs
  software queue           KEY         2000        0ns        0ns         0ns    0.0%
  device                   KEY         2000     6140ns     10.1us     17.58ms    5.1%
  completion -> wakeup     WAKEUP      2000     5190ns     9610ns     10.68ms    3.1%
  residue                  -           2000     39.4us    319.2us    318.93ms   91.9%
  whole read               own clock   2000     74.6us    336.1us    347.19ms  100.0%
  the residue is everything above: syscall entry, the filesystem, submission, the run queue after the wakeup, the return, and the injected work
  reads that reached no device: none
```

And inside `make check`, with the rest of the suite on the same disk:

```
  software queue           KEY         2000        0ns        0ns         0ns    0.0%
  device                   KEY         2000     62.7us    235.4us    138.20ms   27.9%
  completion -> wakeup     WAKEUP      2000     10.8us     17.8us     23.44ms    4.7%
  residue                  -           2000    308.8us    329.8us    333.81ms   67.4%
  whole read               own clock   2000    328.8us    566.6us    495.45ms  100.0%
```

What the two say, and what they do not:

- **The residue is most of a read, and it is not error.** 300 ms of the 319 ms
  alone is the injected work, which is exactly 1000 reads × 300 us and is where
  it should be. What is left — about 19 ms over 2000 reads, some 9 us a read —
  is the syscall entry, btrfs's direct-read path and the submission, the run
  queue after the wakeup, and the return. A per-read split of it measured with
  a Python reader before the tests were written: about 5 us from `pread` being
  called to the request's issue, and about 4 us from the wakeup to `pread`
  returning. That is the segment `fentry` would open, and on a one-page direct
  read on NVMe it is bigger than the device.
- **The device is ten times slower under the suite and the table says where.**
  6 us alone, 63 us beside the rest of the suite. The queue segment is zero in
  both, so none of it is software queueing: the `none` scheduler issues
  directly, and the extra time is inside the device's own queue, which no
  tracepoint in the block layer can see into.
- **The first read after an idle device is slow.** In every run of the Python
  prototype the first request took 4.6–4.8 ms and the rest took microseconds.
  Most likely the device leaving a low-power state —
  `default_ps_max_latency_us` is 100000 on this machine, so autonomous power
  state transitions are on — which is an explanation and not a measurement. The table puts it in the device
  segment, which is right: the device took that long. It is also a reason not
  to read a p99 over a few hundred requests as the device's.
- **Completion to wakeup is 3–5 % of a read**, and it is a kworker's scheduling
  latency plus a checksum. That is the number a reader would otherwise have
  attributed to the device.

### The negative: a read the page cache answers

`a_read_the_page_cache_answers_reaches_no_device`. 2000 buffered one-page reads
of a file whose every page `mincore` says is resident, before the reads and
after them:

```
2000 one-page reads the page cache answered, task:108800 (omnxc108799)
  software queue           KEY            0        0ns        0ns         0ns    0.0%
  device                   KEY            0        0ns        0ns         0ns    0.0%
  completion -> wakeup     WAKEUP         0        0ns        0ns         0ns    0.0%
  residue                  -           2000      640ns     1090ns    1391.8us  100.0%
  whole read               own clock   2000      640ns     1090ns    1391.8us  100.0%
```

All residue, no device segment, and no request credited to any of them. **A
clean run must gain nothing** is the same rule stream E applies to its marks,
and the reason is the same: a decomposition that found a device segment in a
read that never reached one would teach its readers to discount the column.

The positive control is not optional. A window in which nothing happened and a
tool that was not watching look identical, so the same thread then issues 200
direct reads inside the same window, and at least 99 % of them must be found.

---

## The scoped key, against a bare pointer

`BRIEF.md`: *show that no completion was attributed to a request that had
already finished. That check is the whole argument for the key being scoped
rather than a pointer, and it belongs in a test and not only in a document.*

`no_completion_is_credited_to_a_request_that_had_already_finished`, and
`examples/cross-io-stale-pointer.yml`.

Two threads read one-page pages with `O_DIRECT`, each from its own file, **pinned
to one cpu**. That is what makes them share request pointers, and it took a
measurement to know it was needed: blk-mq hands out requests per hardware queue,
NVMe has one queue per cpu, and two readers on two cpus never meet at one
address. On one cpu they recycle each other's requests constantly.

The session watches the issues of one of them and the completions of everybody,
and joins them twice:

```
the reader issued 3000 requests; the scoped join credited it with 2978, the bare
pointer with 29064, of 34135 completions on the machine -- 26086 of them another
thread's, landing on a request the reader had already finished
```

The ground truth is the reader's own count. It issued 3000 requests, so a join
that credits it with more than 3000 has attributed somebody else's completion to
one of its requests, and the scoped join is held to that upper bound exactly.
The bare join is then *required* to exceed it, so the run proves the recycling
happened rather than assuming it — and where no pointer passed between the two
readers, it says so with its numbers and does not claim the half it did not
see.

**The asymmetry is the realistic case, not a contrivance.** A session that
narrows the issuing side — to one process, one cgroup, one device — and leaves
the completing side wide is exactly the session somebody writes for *the latency
of my requests*. It is the session where `eq` is wrong, and wrong by a factor of
ten here.

---

## What cannot be verified here

**An independent lower bound on the device segment.** The reader's clock bounds
every request from above: a device segment cannot be longer than the read it is
in. Nothing on this machine bounds it from below. A collector that reported
half of each latency would pass every check here, and hand the missing half to
the residue. What would close it is a delay put there on purpose — `dm-delay`
under the reader, where the device segment must then be at least the injected
delay — and that needs root, so it belongs with the phase 8 harness beside the
same request in `docs/scenarios/io.md`.

**A software queue that queues.** The only block device here runs `none`, so the
queue segment is zero on every read of every run, and the check that it is
joined to the right life of its pointer is a check on a zero duration with a
real timestamp. The timestamp is what the check uses, so it is not vacuous; but
a distribution of queueing times across a crossing needs `mq-deadline` or
`bfq`, and switching the scheduler needs root.

**The split of the residue.** `sched.runqlat` measures the wait between the
wakeup and the reader being on a cpu, which is part of the residue. It is
bucketed in the kernel per task and per drain window, with no timestamp per
wait, so it cannot be put against one read; and over the run it includes every
other wakeup of the thread. Subtracting it would make the residue smaller and
wrong. What would do it is `sched.runqlat --emit raw` carrying one event per
wait, which the collector does not do on purpose (`docs/families/sched.md` says
why), or `fentry` on the syscall return.

**A write.** The crossing is reads only. An `fsync`ed write is submitted in the
writer's context and would decompose the same way; a deferred one reaches the
device from a kworker thirty seconds later with no submitter anywhere in the
block layer, and no join in this build could put it back on the write that
dirtied the page. `docs/families/io.md` records the split.

**Which work a kworker was doing when it woke the reader.** The test names the
wakers from `/proc/<tid>/comm` after the run, and a kworker renames itself after
the work item it is running: the same worker read `btrfs-endio`,
`events_unbound` and `btrfs-delayed-meta` in three runs. That it is a kworker
and not the interrupt is measured; which queue it served is not, because
`sched.wakeup` carries the wakee's `comm` and not the waker's.

**`sched.runqlat`, `sched.migrations`, `sched.lock`, `proc.syscall` and
`mem.stall` under a thread target.** Defect 2 below was measured on
`sched.wakeup` and `sched.offcpu`. The other five pass `Target::first_pid()` to
a thread-group filter in the same way, which is read from the code and not
measured: a first attempt with a Python thread gave a process-target control
that saw nothing of the thread either, and a control that sees nothing proves
nothing.

---

## What the measurement taught

**`O_DIRECT` is not one request per read on btrfs, until the file is on the
device and out of the cache.** Two findings, both of which read at first like a
collector losing events:

- A file written and read straight back with `O_DIRECT`, before writeback, gave
  **78 requests for 2000 reads**. The data had no extent on the device yet.
- A file whose pages were resident gave **1999 for 2000 in every run**, and the
  missing one was always the first read — whichever page it read. Told to forget
  the file first (`POSIX_FADV_DONTNEED`), the same file gave 2000 for 2000.

So both sessions' one-liners `fsync` the file when they make it and forget it
before reading, and the tests do the same. A scenario that counts requests
against reads, on this filesystem, without both, is counting the page cache.

**A reader that shares a file shares its cache.** The first version of the
scoped-key test had both readers read one file, and found 2960 requests for 3000
reads with nothing dropped: one reader's direct reads were occasionally answered
from pages the other's left resident. A file each gave 2978–2999.

**A block tracer narrowed by `comm` loses its own events to its neighbours.**
`io.latency` and `io.queue` apply a `comm` filter after the drain, which their
own notes say. One run of the decomposition inside a suite that was driving the
disk from another test binary lost **382889** requests to a full ring — the
neighbours' requests filled it before the reader's were picked out. The run said
so and the test did not assert the count it could not have. That is why the
device half of the test now comes from a process-targeted session, which the
block collectors filter in the kernel — and it worked the first time it was
needed: with the machine-wide session dropping 245155 issues and 263394
completions, the process-targeted one found all 2000 of the reader's requests.
The lesson is that a machine-wide request
narrowed by a userspace predicate costs what the whole machine costs, and on a
shared machine that is not the workload's number.

**One target per session made the crossing two runs.** `io.queue` has only a
`system` target, a session observes one target, and `sched.wakeup` cannot be
narrowed by `comm` (defect 1). So the test runs a machine-wide session for the
queue half and the scoped join, and a process-targeted one for the device half
and the wakeups, over the same window, and joins their raw events itself. Two
program sets for `io.latency` agreeing on `blk:rq@open_ts` is the property
`docs/scenarios/io.md` scenario 11 established, and it is what makes the two
runs joinable at all.

---

## Defects found

Four, none of them worked around in `src/`, each reproducible with the command
given. The signatures that would fix them are under "Stream P3" in the "Needs
from another stream" section at the end of `docs/open-findings.md`.

### 1. `sched.wakeup` and `sched.offcpu` accept a filter and ignore it, silently

```console
$ omn sched.wakeup --system -m trace -e raw -d 300ms --filter "comm == 'omn-no-such-task'" --format json
  # 1528 of 1528 events belong to other tasks, and no degradation mentions the filter
$ omn sched.offcpu --system -m trace -e raw -d 300ms --filter "comm == 'omn-no-such-task'" --format json
  # 352 of 352
```

The predicate is validated against the catalog's filter fields, the resolver
places it, and neither collector evaluates it. Compare the block collectors,
which apply the same predicate and say so on every run — *"the filter kept 199
requests and removed 15; it ran here, over the drained records, not in the
kernel"*. A summary of wakeups under `--filter "comm == 'omnreader'"` is
therefore the machine's, labelled as the reader's. It is the same shape as
finding 4 of `docs/families/io.md` two waves ago, and it is why
`examples/cross-io-one-read.yml` carries no filter on its wakeup request and
says why in its header.

`the_scheduler_half_ignores_a_filter_and_says_nothing` holds it: it asserts the
defect, so the day the filter is applied it fails and names what to flip.

### 2. A thread target on the scheduler half kept nothing, and reported no loss — **fixed**

**What closed it**: in the thirteenth wave the `sched` and `proc` programs compare
the thread for a `tid` target and the thread group for a `pid` target
(`src/bpf/insn.rs`, one comparison every program uses).

```console
$ omn sched.wakeup --tid <a thread that is not its process's first> -m trace -e raw -d 2s
  # 0 events; losses all zero
$ omn sched.wakeup --pid <its process> -m trace -e raw -d 2s
  # 66 events for that thread, which slept 60 times
```

`WakeupCollector` and `OffCpuCollector` take `Target::first_pid()`, which for a
`tid` target is the thread id, and hand it to a program that compares it with
the task's `tgid`. The two are equal only for a process's first thread — the
one case every earlier test used, since a launched program's main thread is the
one they watched. For every other thread the filter matches nothing, the run
succeeds with zero events and a clean loss line, and the reader is told the
thread never blocked and was never woken.

The block collectors do this right (`Submitter::Thread` compares the thread id),
so the fix has a model beside it.
`a_thread_target_on_the_scheduler_half_keeps_that_threads_events` holds the
fix with its positive control: the process target must see the thread's
wakeups, or the test says it proved nothing.

### 3. `eq` on a scoped key was accepted at the same evidence as `scoped_eq` — **fixed**

**What closed it**: the bare pointer's join is rated `TEMPORAL` now, as this
crossing offered. It still runs, because it is still a correlation; what it no
longer does is claim the evidence of the scoped key it ignores.

```
join scoped  io.queue -> io.latency  evidence KEY
  2997 of 15001 `io.latency` events attributed (20.0 %)
join bare  io.queue -> io.latency  evidence KEY
  14493 of 15001 `io.latency` events attributed (96.6 %)
```

11496 completions credited to a request that had already finished, by a join
the engine rates exactly as highly as the one that got it right. `check()`
refuses `scoped_eq` on a key without a scope, which is the right refusal; the
converse is not refused at all, and `evidence()` derives the level from the key
kind and the match rather than from whether the match can tell two lives of the
key apart. On `blk:rq@open_ts`, `eq` compares the pointer alone, which is the
comparison `docs/scenarios/io.md` scenario 10 measured as meaningless: hundreds
of pointers, a hundred thousand requests.

`examples/cross-io-stale-pointer.yml` declares it on purpose to show it. Nothing
else should be able to.

### 4. The wakeup join's "heaviest chains" were not ranked by anything — **fixed**

**What closed it**: the same change as the compute crossing's finding 6 --
chains are grouped and ranked by what they released.

Each wakeup opens its own state, and a completion or a block matches at most the
one open state of its task, so against `io.latency` every attribution has a
count of 1. The table titled *heaviest wakeup chains* is then the first `top`
entries in key order: in one run it printed `task:0 (idle) -> task:113142` as
the heaviest. That run did not count the wakers; every run since that has,
counting the wakeup that followed each completion, found three kworkers
leading and the idle task not among them. The
engine keeps one attribution per state; what a reader wants from the table is
the attributions **grouped by chain**, with the count of each.

And at `depth: 3` the chain states its weakness in a way that is easy to read
past. One run of the example session, beside the test suite, printed

```
... -> task:100436 -> task:120808 -> task:64368 -> task:120834
```

where `120834` is the example's Python reader, `64368` the kworker that woke it,
and `120808` a thread of *this test binary* — a different reader in a different
process, credited with waking the kworker because the interrupt that did so
arrived while it was on the cpu. The `sched.wakeup` caveat says exactly this;
the chain is where it becomes a sentence a reader would believe.

---

## Running these

```console
make                                   # the capabilities, and the workspace
cargo test --test cross_io -- --nocapture
make check                             # everything, the way CI would
```

Six tests, about ten seconds, of which most is wall-clock windows: each
measurement gives its sessions a second to attach before the reads start and
holds the window open 400 ms after. They run in parallel with each other, and
every reader renames its thread to a name built from the test's letter and the
process id, so two tests — or two checkouts running their suites at once — never
narrow onto each other.

Where this machine cannot serve a test, it stops and names why: a build directory
that is not on a block device, or is on a compressed filesystem; a binary without
`CAP_BPF` and `CAP_PERFMON`; a filesystem that refuses `O_DIRECT`; a cpu the two
readers cannot both be pinned to; no `python3` for the example sessions'
workloads. And where a source admits a loss, the count it could not have is not
asserted, and the run says which one and by how much — which, for the example
session run inside the suite, happened: a machine-wide block half narrowed by
`comm` matched 142 of 2000 while other suites drove the disk. That run's drop
count was not kept; the decomposition test's own machine-wide session, in a
run like it, dropped 263394.

Two lower bounds are separations rather than thresholds, on purpose. The scoped
join has to find at least half of the reader's requests — the failures that
bound exists for credit nothing at all — and not 99 %, because the reads btrfs
answers without a request vary with the machine: 2960 to 2999 of 3000 over the
runs here, one of which fell below 99 % with nothing dropped. The upper bounds
are exact, and they are the claims.
