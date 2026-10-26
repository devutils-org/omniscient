# The compute crossing

Where one task's wall clock went: on a cpu, waiting for one, or blocked, and
on what. This is stream P2 of the seventh wave, and it is where `docs/ROADMAP.md`
phase 10 starts: thread state analysis answers *why is this slow*, which is the
question people arrive with, where USE answers *what is saturated*.

The sessions are `examples/cross-compute-*.yml` and the checks are
`tests/cross_compute.rs`. Every number here was read off this machine -- 24
cores, kernel 7.2.6 built with `CONFIG_IRQ_TIME_ACCOUNTING` and `HZ=1000`,
`CAP_BPF` and `CAP_PERFMON` on the binary -- and most of them while four other
streams were running their suites on it, at a load average of 20.

Nothing here is new mechanism, and no file under `src/` changed. Every source
the crossing joins already collects and is already checked on its own. What is
new is the arithmetic that puts them in one table, and what that arithmetic
found the table was missing.

---

## The one idea

A task's life is spent in three states, and each has a source:

| State | Source | Kind of number | Resolution |
|---|---|---|---|
| on a cpu | `cpu.profile` | **sampled**: a count of samples, each standing for one period (1.003 ms at 997 Hz) | a count's ±1, times the period |
| runnable, waiting for a cpu | `sched.runqlat` | **traced**: the exact sum of every wait, kept in the kernel | nanoseconds |
| blocked | `sched.offcpu` | **traced**: the exact sum of every block, kept in the kernel | nanoseconds |

plus `cpu.oncpu_time`, the kernel's own cpu accounting read on a 50 ms poll in
10 ms ticks -- **polled**, a third kind -- as a second opinion on the first
row. The brief's warning is the first column of that table: a sampled profile
and a traced duration are not the same kind of number, and a table that puts
them side by side says which is which.

**The ground truth is the task's own kernel accounting.** Every workload here
reads `/proc/self/schedstat` and `/proc/self/status` before it exits and prints
them on stderr, prefixed `omn-truth`: nanoseconds on a cpu, nanoseconds waiting
for one, timeslices, voluntary switches, and its own clock around the part that
matters. The tool reads none of those files and none of that line. So each row
is checked against the kernel's figure for the same quantity of the same task,
through a mechanism the tool does not use -- the `sched_info` accounting the
scheduler keeps for itself, against raw tracepoints and a `cpu-clock` perf
event.

---

## The decomposition, and its residue

From `examples/cross-compute-where-the-time-went.yml`, the half-blocked
workload on an otherwise busy machine:

```
where the time of task:117505 went
  STATE      SOURCE                       KIND      EVIDENCE        MS  SHARE
  on cpu     cpu.profile x 1.003 ms       sampled   KEY          196.6  55.2 %
  runnable   sched.runqlat                traced    KEY            1.4   0.4 %
  off cpu    sched.offcpu                 traced    KEY          163.9  46.0 %
             of which queued after a wakeup, and also counted as runnable:
             between 0 and 1.4 ms; no output says how much
  parts                                                          361.9
  whole      the workload's clock .. the run's window        356.1 .. 392.5
  RESIDUE    whole - parts                                    -5.8 .. +30.6
  second opinion on the first row: cpu.oncpu_time, polled every 50 ms in 10 ms ticks, 180.0 ms
  interrupted: the sampled row counts the interrupts that ran while the task
  held its cpu and the kernel's on-cpu figure does not; the difference is +3.0 ms
  the kernel's own: on cpu 193.6 ms, runnable 0.25 ms, 612 voluntary switches;
  the tool saw 612 blocks and 637 waits
```

Every row agrees with the kernel: 612 blocks against 612 voluntary switches,
exactly; 196.6 ms of samples against 193.6 ms on cpu; the runnable row within
the per-wait difference explained below. The workload is half on the cpu and
half blocked by construction, and comes back 54 % and 46 %.

**The tool does not print this table.** There is no decomposition in the
engine, and a launched program's lifetime is in no output, so `render` in
`tests/cross_compute.rs` builds it from the JSON and the whole is a range: the
workload's own clock below it, the run's window above. The requests for both
are under stream P2 in `docs/open-findings.md`. What a session prints today is
the three sources one after another, which is correct and does not add up, and
the header of every session says so.

**The residue is printed as a range rather than rounded away**, because each
time it was wrong-signed or too large it was hiding a state. Three of them,
each below: a negative residue is time counted twice, and a positive one is a
state no row covers.

---

## What the residue found

### 1. `sched.offcpu` contained the run queue wait after every wakeup, which `sched.runqlat` also counts — **fixed**

`src/bpf/offcpu.rs` times a block from the `sched_switch` that took the task off
the cpu to the `sched_switch` that gave it one back. A task that is woken and
then waits for a turn is still off the cpu, so that wait is inside the block --
and `sched.runqlat` opens a wait at the wakeup and counts it too. The catalog's
caveat said *"blocked, not preempted"*, which is true: the preempted case is
excluded. The woken-and-waiting case was not, and nothing said so.

**What closed it**, in the eighth wave's coordinator pass: a second program on
`sched_wakeup` -- the same tracepoint `sched.runqlat` measures from, so the two
talk about one wait -- stamps the moment into the in-flight entry the
`sched_switch` program created. Every row now carries `queued_ns` and
`blocked_ns` beside `total_ns`, and `blocked_ns + queued_ns == total_ns` is
arithmetic rather than an estimate. Checked against the kernel's own `run_delay`
for the loop in this crossing's own test: **93.6 ms reported against the
kernel's 96.5 ms**, under 30 microseconds per wakeup apart, on an independent
counter rather than a rearrangement of the tool's own numbers. A `queued_ns` of
zero on a block whose wakeup the run did not see means unknown rather than none,
and the run says how many blocks that is.

`examples/cross-compute-queued-after-wakeup.yml` measures it against a number
nobody has to take on trust. A Python process sleeps 100 times for 5 ms, in
`SCHED_BATCH` beside a spinning shell on one cpu -- a batch task that is woken
does not preempt, so it waits for the next tick -- and reports what the kernel
says about the loop:

| | run 1 | run 2 | run 3 | run 4 |
|---|---|---|---|---|
| requested sleep | 500.0 | 500.0 | 500.0 | 500.0 |
| `sched.offcpu`, 100 blocks | 582.4 | 578.8 | 580.7 | 582.2 |
| the loop's clock minus its cpu | 582.5 | 578.9 | 580.8 | 581.4 |
| the kernel's queue wait in the loop | 77.0 | 73.6 | 75.6 | 75.8 |
| `sched.runqlat`, whole process | 84.35 | 83.13 | 83.94 | 83.66 |
| the kernel's `run_delay`, whole process | 84.43 | 83.21 | 84.01 | 83.68 |

`sched.offcpu` is the loop's wall clock to a tenth of a millisecond: the
requested sleep, plus five milliseconds of timer slack, plus every nanosecond
the kernel says the task then waited for a cpu. Those same nanoseconds are in
`sched.runqlat`, which matches the kernel's `run_delay` to 0.1 %. A
decomposition that adds the two rows counts 75 ms of a 580 ms loop twice.

`examples/cross-compute-three-states.yml` shows what that does to the table
when all three states are large -- the driver, its responder and a spinning
shell share one cpu:

```
  on cpu     cpu.profile x 1.003 ms       sampled   KEY          205.6  29.4 %
  runnable   sched.runqlat                traced    KEY          248.6  35.5 %
  off cpu    sched.offcpu                 traced    KEY          315.7  45.1 %
  parts                                                          769.9
  whole      the workload's clock .. the run's window        700.2 .. 774.4
  RESIDUE    whole - parts                                   -69.7 .. +4.5
  the kernel's own: on cpu 200.2 ms, runnable 248.45 ms, 406 voluntary switches;
  the tool saw 408 blocks and 626 waits
```

The shares add to 110 %. Every row still matches the kernel -- `sched.runqlat`
reads 248.6 ms against a `run_delay` of 248.45 ms -- and the parts still exceed
the workload's own clock by 56 to 71 ms over five runs. The upper end of the
range reaches zero only because the run's window is loose (finding 4).

**Neither row is wrong**, and the fix is not to change either answer. What is
missing is the split: `sched.offcpu` already sees both ends of every block, and
`sched.runqlat` already hooks `sched_wakeup`. A `queued_ns` beside `total_ns`
on each `sched.offcpu` row is the whole request, and it is in
`docs/open-findings.md` with the signature.

What the tests assert: the queue wait is inside `sched.offcpu` (it equals the
loop's clock minus its cpu, within the timer's slack), the part that is not
sleep equals the kernel's queue wait, and `sched.runqlat` holds it too. What
they do not assert is that the three-states residue is negative, because the
window that bounds it is loose by more than the overlap on some runs.

### 2. A fourth state: the task held the cpu and an interrupt had it

A `cpu-clock` sample fires on the time a task holds its cpu, and that time
includes the interrupts that ran on the cpu meanwhile. The kernel's own on-cpu
figure, `sum_exec_runtime`, leaves interrupt time out on a kernel built with
`IRQ_TIME_ACCOUNTING`, which this one is and most distribution kernels are. So
the first row and its reference measure different things, and the difference
is a state no source here reports.

Measured on a shell spin loop, against the loop's own wall clock:

| Rate | Wall | Kernel on cpu | Wall - on cpu - runnable | (recorded + dropped) x period |
|---|---|---|---|---|
| 997 Hz | 1063.5 ms | 1056.1 ms | 6.9 ms | 1063.2 ms |
| 10 000 Hz | 1105.5 ms | 1067.1 ms | 36.3 ms | 1104.0 ms |
| 50 000 Hz | 1337.7 ms | 1138.5 ms | 198.8 ms | 1337.3 ms |
| 100 000 Hz | 1725.2 ms | 1234.3 ms | 488.7 ms | 1716.1 ms (783 throttled) |

The samples, dropped ones included, are the wall clock. The kernel's figure is
the wall clock minus the interrupted state, which is 0.7 % at 997 Hz and 28 %
at 100 kHz -- because at that rate the interrupts are the profiler's own. That
is the observer effect, and it lands in no state of the kernel's accounting: a
reader who adds the kernel's on-cpu, `run_delay` and blocked time for a heavily
profiled task finds a quarter of its life missing. The kernel's on-cpu figure
rises as well, 1056 to 1234 ms for the same loop, which is what the cache
traffic of sampling costs the task in its own context.

At 997 Hz the interrupted state is 2.6 to 5.4 ms of 200 across the runs here,
within the first row's slack. The table prints it on its own line rather than
folding it into the slack, because at a higher rate it is the largest thing in
the table.

An earlier reading of the same sweep compared recorded samples with the
kernel's on-cpu figure only, concluded that a sample stands for *less* than a
period at high rates, and wrote that a lossy run's mark "points the wrong way".
It does not: the mark is right about what samples count, and the comparison was
between two definitions of on-cpu. The wall clock is what settled it.

### 3. A lossy run beside a clean one: what the mark changes

`examples/cross-compute-lossy-profile.yml` is the first session with the
profile at 100 kHz, past `kernel.perf_event_max_sample_rate`, so samples are
throttled and the ring overflows:

```
clean                                              lossy
  on cpu     sampled   197.6  55.1 %                 on cpu     sampled   267.3  51.7 %
  runnable   traced      1.7   0.5 %                 runnable   traced      2.2   0.4 %
  off cpu    traced    172.4  45.7 %                 off cpu    traced    179.8  34.8 %
  parts                383.8                         parts                449.3
  whole      377.5 .. 446.5                          whole      516.9 .. 599.0
  RESIDUE     -6.2 .. +62.7                          RESIDUE    +67.7 .. +149.7
  interrupted          +3.6 ms                       interrupted          +29.7 ms
                                                     the profile says: short by at least 6399
                                                     of 33126 events (19.3 %), and by an
                                                     unknown number more: throttled 41 times
                                                     with the 6399 dropped samples counted
                                                     back in: residue +3.7 .. +85.7
```

The mark changes the one row it is on. The lossy residue turns positive by
64 ms -- a state appears to be missing -- and the mark is what accounts for it:
6399 samples of 10 µs is 64 ms, and with them counted back in the residue
closes to +3.7 ms, which is the 41 throttled windows whose content nobody
recorded. The two traced rows are totals kept in the kernel and lose nothing:
611 blocks against 611 voluntary switches in the lossy run as in the clean one.
And a clean run gains nothing -- no mark, no line -- which the tests assert,
because a mark on a clean run is noise that teaches readers to skip it.

Two things the mark cannot yet do. It is on `hist` only: `topk`, which is the
emit that carries the sample count a reader would use, renders a lossy count
exactly like a clean one until stream E lands (both sessions request `hist`
for that reason alone -- a histogram of samples that each count one is
otherwise not worth reading). And the lossy workload itself is 37 % slower by
its own clock, 517 against 378 ms, which is finding 2 again: the cost of
sampling is paid by the task.

### 4. A launched program's lifetime is in no output

The whole of the decomposition is the task's lifetime, and nothing the tool
emits carries it. `observed_ns` is the run's window, from releasing the program
at the gate to the last drain after it exited, and it is loose by however late
that drain was: 14 to 146 ms past the workload's own clock across the runs
here, and the most when the machine was busiest. The
workload's own clock is a lower bound that misses the shell's start and its
last few statements. So the whole is a range, the residue is a range, and a
negative residue can be proven only when the overlap is larger than the
window's slack -- which is why finding 1 is proven in the queued session and
only shown in the three-states one.

`src/collect/launch.rs` already holds both ends: the release, and the reap.
The request is for them in the results.

### 5. A wakeup join onto `sched.offcpu` attributed drains, at WAKEUP — **refused now**

**What closed it**: the eighth wave's join engine refuses the rule at
validation. The catalog marks `sched.offcpu` and `sched.runqlat` as `drained` --
each event holds many blocks -- and a wakeup join onto a drained total is
refused with that reason, before anything is attached, rather than attributing
drains and calling it `WAKEUP`.

The seventh wave's brief (`docs/dev/briefs/wave-7-crossings.md`) names `sched.wakeup -> sched.offcpu` for "what released each
block". A `sched.offcpu` event is a total drained every 100 ms and stands for as
many blocks as its `blocks` attribute says -- 159, 171, 169 and 101 for the pipe
read here. A wakeup join credits each `to` event to the most recent wakeup of
that task, so each drained total, every block in it, goes to whichever wakeup
came last before the drain.

`examples/cross-compute-drained-wakeup.yml`: 11 events standing for 607 blocks,
reported as `11 of 11 sched.offcpu events attributed (100.0 %)` at evidence
WAKEUP. The responder released the driver 602 times; the last drain's hundred
pipe blocks, every one released by the responder, were credited to a
short-lived child the driver waited for after its loop. The level printed is
WAKEUP, and what it rests on is two things happening close together in time,
which is TEMPORAL -- the failure the evidence ladder exists to prevent, printed
by the engine itself.

The crossing uses `sched.wakeup -> sched.switches` instead: one
`sched.switches` event is one switch, 629 of them against the kernel's 629
timeslices in one run, and every one is credited to the wakeup that began the
run it ended (`629 of 629 attributed`; the test allows three before the first
wakeup). That answers
*what released each run*, which for a ping-pong is the same count as what
released each block, shifted by one.

The test pins the defect in the one form that does not depend on timing -- the
join's unit is the event, and each event holds at least ten blocks -- so that
the day the engine refuses the rule or weighs the event, the test fails and
points here. The two ways to fix it are in `docs/open-findings.md`; the one
this project's invariants favour is a refusal, because *an invalid combination
is refused with the reason, never degraded silently*.

### 6. The wakeup table ranked wakeups, not chains — **fixed**

**What closed it**: wakeup attributions are grouped by chain and ranked by
what they released, which is what a reader deciding where to look next wants.
`examples/join-decompose-compute.yml` puts the responder at the top.

`heaviest wakeup chains` lists attribution states, and a wakeup join opens one
state per wakeup. For 1230 wakeups that is 1230 states of one or two events each,
and the top five are whichever wakeups happened to precede a preemption or two:

```
    CHAIN                                          EVENTS  OUTCOME
    task:67954 -> task:67955                            5  ran
    task:67962 -> task:67955                            4  ran
    ... -> task:67963 -> task:67955                     3  ran
```

The question the table is headed with -- which task releases this one most --
has an answer (the responder, 602 times) that no row shows. The request is for
`assemble` to merge a wakeup join's attributions by chain before ranking.

A second, smaller thing on the same table: a state still open at the end of the
run is rendered `never ran` even when events were attributed to it, which means
the task did run. `still open at the end` is what happened.

### 7. `sched.runqlat` and the kernel's `run_delay` differ per wait, by the path the wakeup took

When every wait is behind another task on one cpu, the two agree to 0.07 %:
229.95 against 229.78 ms over 623 waits, 0.3 µs per wait. When every wakeup
lands on an idle cpu they do not: 1.28 against 0.19 ms over 631 waits, 1.7 µs
per wait, six times the kernel's figure. Both are small next to anything a
decomposition is read for, and both tests carry 5 µs per wait as slack.

The probable cause, not proven here: a wakeup that preempts sets
`rq_clock_skip_update`, so the kernel's run queue clock is not updated at the
switch that ends the wait and its `run_delay` stops early, while the tool reads
`bpf_ktime_get_ns` at both tracepoints. An idle cpu is always preempted by a
wakeup; a busy one mostly is not. It is written down because it is the kind of
difference that reads as a bug in whichever tool the reader trusts less.

### 8. `cpu.oncpu_time` on a launched program is short by its last poll — **the exact figure is carried now**

The polled second opinion read between 170 and 190 ms against the kernel's
190.3 to 206.0 across the runs here: always a multiple of ten, always short, by
10 to 26 ms. Each reading is the delta of a tick counter since the last poll, and the
cpu the task spent between the last poll and its exit is in no reading. That is
the open question `docs/dev/STATE.md` names -- *the closing read of a polled counter* --
met on a third family. For a launched program the reap already has the exact
answer: `wait4` returns the child's `rusage`. The test holds the row to one poll
and one tick of the kernel's figure and no closer.

**What landed**, in the eighth wave's coordinator pass, and it is deliberately
not a closing read. `Collector::stop`'s own documentation says why: the runner
closes the window itself, so a further read there produces a second point at
the same offset with a value near zero and drags every summary computed over the
series. What the reap has is not another point, it is the **total** -- so it
rides on the lifetime, beside the whole that a decomposition's parts are parts
of, and the series stays the series it is.

Measured by `the_reap_carries_the_cpu_the_last_poll_missed`: a busy loop asked
for a second of cpu, the polled series summed **990.0 ms** -- a round multiple
of the tick -- and the reap handed over **1006.5 ms**. The test asserts the two
numbers against each other and against their own resolutions, never either one
on its own: how much cpu a busy loop gets belongs to the machine, and that the
polls and the reap agree to within a poll and a tick belongs to the tool.

The open question itself is not settled by this. `net` takes a closing read,
`io` and `power` deliberately do not, and a launched program is the one case
where the kernel volunteers the answer without being asked again.

---

## Evidence, row by row

| Result | Evidence | Why that and not more |
|---|---|---|
| each row of the decomposition | **KEY** | every source is filtered to the launched thread group in the kernel and carries `task:tid`; the rows are the same task because they share that key, and nothing propagated an identity between them |
| `what-released-each-run`, wakeup onto switches | **WAKEUP** | one wakeup edge per switch; the waker is whoever was running when `sched_waking` fired, which the source's own caveat says is evidence and not proof |
| `what-released-the-block`, wakeup onto offcpu | printed **WAKEUP**, worth **TEMPORAL** | finding 5 |
| the whole | none | a bracket, not a join: the workload's clock and the run's window |

**No `eq` join ties `cpu.profile` to `sched.offcpu`.** One was tried: an `eq` on
`task` between two sources filtered to the same thread group reports
`15 of 15 attributed, evidence KEY`, which is true and says nothing -- every
event of both sides has the one key there is. The table states KEY per row
instead of dressing a filter up as a correlation.

---

## Scenarios

Every one runs in `tests/cross_compute.rs`, one at a time: two of them pin a
spinning shell to a cpu on purpose, and a crossing run beside another is its
confounder.

| Session | What it checks | Against |
|---|---|---|
| `cross-compute-where-the-time-went.yml` | each row matches the kernel; the parts bracket the task's clock; the halves are equal; a clean run carries no mark | `schedstat`, `voluntary_ctxt_switches`, the loop bound written twice |
| the same, its join | every switch is attributed or counted; switches equal the kernel's timeslices; every chain ends at the switched-out task | `schedstat`'s timeslice count |
| `cross-compute-three-states.yml` | with all three states over a tenth of the task's life, each still matches the kernel and the parts still bracket the clock | `schedstat`; the residue is printed, not asserted negative |
| `cross-compute-queued-after-wakeup.yml` | `sched.offcpu` is sleep plus the kernel's queue wait, and `sched.runqlat` holds the same wait | 500 ms written into the program, and the loop's own `schedstat` |
| `cross-compute-lossy-profile.yml` | a lossy profile marks its row with exactly the losses the run reported; the traced rows lose nothing; with the dropped samples counted, the parts still fit the window | the run's own loss counters, `schedstat` |
| `cross-compute-drained-wakeup.yml` | the wakeup join onto `sched.offcpu` counts drained events, each holding at least ten blocks | `voluntary_ctxt_switches` |

Each row's slack is its reference's resolution and nothing fitted to this
machine: a sample count's ±3 periods plus 5 % for the interrupted state; one
poll and one tick for the polled row; 5 µs per wait for `run_delay` (finding
7); 1 % plus three for blocks against voluntary switches (a task about to sleep
with a signal pending is read as blocked and put back, as `src/bpf/offcpu.rs`
says); 100 µs per sleep for timer slack.

---

## Workloads

**The half-blocked ping-pong** -- `where-the-time-went`, `lossy-profile`,
`drained-wakeup`. `sched-half-blocked.yml`'s shape: 600 round trips, 100 spins
before each answer on both ends. Guarantees the driver's on-cpu and blocked
time are equal by construction, 600 blocks on the pipe, and one wakeup per
round trip from the responder. About 0.36 s by its own clock, 0.52 s under the
100 kHz profile. Reports `tid`, `wall_ns`, `oncpu_ns`, `runq_ns`, `slices`,
`voluntary`. The driver is `/bin/sh`, single-threaded, so its process and its
task are one.

**The same, contended** -- `three-states`. Pinned with `taskset` to the middle
cpu of those this process may use -- not the last, which `tests/family_sched.rs`
contends on purpose, and a suite beside it on the same cpu measures it -- beside
a spinning shell started under `timeout 20` and killed on the way out. Guarantees all three states above a tenth of the task's
life: about a third each. Needs `taskset` and `timeout`.

**The queued sleeper** -- `queued-after-wakeup`. `python3` sleeping 100 times
for 5 ms in `SCHED_BATCH`, beside a spinning shell on the same middle cpu.
Guarantees
500 ms of requested sleep, 100 blocks, and a queue wait of about 75 ms after
the wakeups, which the program measures itself. Reports the loop's clock, cpu
and queue wait apart from the process's. Needs `python3`, `taskset`, `timeout`.

---

## What cannot be verified here

- **Anything without `CAP_BPF` and `CAP_PERFMON`.** Every session crosses an
  eBPF source. The tests skip, saying so and saying what they would have
  checked; a green suite on an unprivileged binary says nothing about this
  crossing.
- **A machine where the spinning shell does not make the task wait.** The
  three-states test stops, with the numbers, if the runnable row is under a
  tenth of the task's life, and the queued test if the kernel's queue wait is
  under four times the timer's slack. Neither happened here in any run.
- **A kernel that delivers 100 kHz without loss.** The lossy test stops, with
  the sample count, if nothing was dropped or throttled.
- **The exact residue.** Finding 4: until the lifetime is in the output, the
  residue is a range, and the tests assert only what a range can prove.
- **A multi-threaded task.** Every workload here is single-threaded on
  purpose. `cpu.profile` follows the target task only -- a sampling event is
  not inherited, and the run says so -- while `sched.runqlat` and
  `sched.offcpu` filter by thread group in the kernel. A decomposition of a
  multi-threaded *process* built from today's rows would put one thread's
  on-cpu beside every thread's blocked time, which is why this is a
  decomposition of one task and says so.
- **A kernel without `IRQ_TIME_ACCOUNTING`.** There the kernel's on-cpu figure
  includes interrupt time, finding 2's state folds into the first row, and the
  `interrupted` line reads near zero whatever the rate.

---

## Where phase 10 starts, and what it needs first

Thread state analysis for one task works with what exists, in a test. For it to
be a thing the tool does rather than a thing a test computes, it needs four of
the seven requests under stream P2 in `docs/open-findings.md` -- the other
three are the polled row's closing read, the catalog caveat for finding 2, and
the `never ran` wording -- in the order they are worth doing:

1. ~~**`queued_ns` on every `sched.offcpu` row** (finding 1)~~ — **landed**.
   The double count was the one thing no amount of engine work downstream could
   have fixed, so it went first: `total_ns - queued_ns` is the blocked row, and
   the queue wait it takes out is the one `sched.runqlat` reports.
2. ~~**The launched program's lifetime in the results** (finding 4)~~ --
   **landed** in the eighth wave, as `whole: lifetime`.
3. ~~**A decomposition in the engine**~~ -- **landed** in the eighth wave: a
   `decompose:` block beside the joins, a row per state with its kind and
   evidence, and a signed residue that is never rounded away. The
   `interrupted` row is still not built: it needs the kernel's own on-cpu
   figure as a source, and nothing reads `schedstat` yet.
4. ~~**A wakeup join that refuses a drained `to` side** (finding 5), and ranks
   by chain (finding 6)~~ -- **landed** in the eighth wave; the refusal is at
   validation, and `examples/cross-compute-drained-wakeup.yml` carries it.

And the `never ran` wording is gone: a chain with events attributed to it is
`still open at the end` (`src/encode.rs`). The tool now produces this table
itself -- `examples/join-decompose-compute.yml` -- and `omn latency` asks for
it as a question since the tenth wave.

Then the objective. `docs/dev/STATE.md` puts it as a request parameter -- *limited for
what* -- and this crossing shows why it has to come before any verdict: the
three-states task is 29 % on cpu, 35 % runnable and 45 % off cpu, and whether
that is a cpu problem, a contention problem or a blocking problem depends on
whether the reader wants its median, its tail or its throughput. A
decomposition is the input to that judgement, not the judgement.

---

## Handoff

- **Done**: five sessions and eight tests, all passing on this machine with the
  capabilities, repeatedly and at load averages up to 20. The last `make check`
  was clean end to end: build, every suite, clippy and fmt.
- **Seen on the way, and not this stream's**: two earlier `make check` runs on
  the shared machine failed elsewhere while other streams ran their suites. One
  was `tests/scenario_mem.rs` -- three tests, one of which
  (`a_gibibyte_of_reads_costs_one_l1_miss_per_cache_line`, a TLB count that
  depends on the kernel finding huge pages) also failed when run alone. The
  other was `tests/family_sched.rs`, whose `PinnedSpinners` contends the last
  cpu and found a task "alone" on it waiting 32 % of the window: a second suite
  on the same cpu. Both passed on rerun, and no file under `src/` or any `mem`
  or `sched` path changed here. That second one is why the two sessions here
  that pin a spinning shell pin it to the middle cpu, not the last.
- **Not done, and why**: the decomposition is computed in the test, not the
  tool, because the tool has nowhere to compute it; the requests above are
  what it needs. The wakeup join is onto `sched.switches` rather than the
  `sched.offcpu` the brief named, for finding 5.
