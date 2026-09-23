# Scenarios — crossed families, and the machine as a confounder

Stream P4 of the seventh wave. The question is the one that invalidates other
people's measurements: **does the machine's own state explain what you just
measured?**

Every source joined here already collected and was already checked against
ground truth before this wave. Nothing below is new mechanism. What is new is
the arithmetic that puts two families in one table, a check of that
arithmetic against a condition the test created on purpose, and an account of
what the table cannot say.

This crossing produces no headline number. Its output is a verdict. Where a
decomposition exists, the verdict is its parts plus a residue, printed. Where
the evidence is only that two things happened at the same time, the verdict
says `TEMPORAL` and nothing stronger.

Read `BRIEF.md` (stream P4) for the mission, `docs/scenarios/power.md`,
`cpu.md`, `mem.md` and `io.md` for the workloads reused here, and
`examples/allocation-cost.yml` for the join these started from.

---

## The crossings

| Question | Session | The condition the test creates | Evidence | Checked by |
|---|---|---|---|---|
| Did the clock explain the instruction rate? | `cross-machine-frequency.yml`, plus a per-pid counter session beside it | the same spin loop, with every other core loaded (clock moves, pipeline does not) and with its SMT sibling released (pipeline moves, clock does not) | **TEMPORAL** | `the_cycle_counter_and_the_frequency_agree_on_a_core_held_busy`, `a_slowdown_from_the_clock_is_put_on_the_clock_and_one_from_the_sibling_is_not` |
| Did this run throttle? | none shipped; a written session in the test | none: throttling cannot be made on demand, and this machine cannot see it | — (refused) | `a_guardrail_this_machine_cannot_serve_refuses_the_whole_crossing` |
| Was the I/O the program's, or the memory limit's? | `cross-machine-reclaim-io.yml` | one program run twice, once in a cgroup capped at 32 MiB and once uncapped | **IDENTITY** for the reads, **KEY** for the reads tied to a reclaiming task, **TEMPORAL** for the writes and the device total | `a_memory_limit_turns_a_cached_read_into_device_reads_and_each_byte_carries_its_rung` |
| Does every shipped session run, or name what it needs? | both | — | — | `every_cross_machine_session_validates_and_runs_or_names_what_it_needs` |

---

## 1. The clock under an instruction rate

### The arithmetic

An instruction rate is three factors multiplied together:

```
instructions / wall  =  instructions / cycle   x   clock   x   cycles / (wall x clock)
                         IPC: the code and          the         residue: the share of
                         what shares its core       machine     the window the program
                                                                had its core
```

The identity is exact by construction, so the useful question is where each
factor's number comes from. The answer is two different instruments:

- **IPC** is `cpu.instructions / cpu.cycles`: the same PMU read twice over the
  same window. The ratio is exact.
- **The clock** is `power.freq`, which on this machine reads
  `cpuinfo_avg_freq`. The processor derives that value from aperf and mperf,
  and it knows nothing about the cycle counter. Every event states whether it
  measured the `effective` clock or only the `requested` one, and the test
  refuses to decompose on a requested frequency. The governor's request
  explains nothing about a slowdown.
- **The residue** is what remains. It is printed for every run and never
  rounded away.

Comparing two runs, the logarithms of the factor ratios add up to the
logarithm of the rate ratio. The test prints the change as
`rate = clock + IPC + residue` in percent, so a reader can see that every
point of the change is accounted for.

### What the test creates, and what it found on the way

The same spin loop runs on core 1 in four conditions:

| Condition | What changed, by construction | What the tool must say |
|---|---|---|
| **paired**: the loop, plus a second loop on its SMT sibling | the reference | — |
| **and ten other cores**: both threads of every other physical core but one | the package's boost budget | the clock moved and the IPC did not |
| **sibling released**: the loop alone on its core | the core's issue width | the IPC moved and the clock did not |
| **paired, again** | nothing | the run-to-run movement of each factor, used as slack |

The first version of this test was wrong twice, and both mistakes are part of
the result.

**First, the reference left the SMT sibling idle.** On a shared machine, the
scheduler places other work on idle hardware threads, and an idle sibling is
one of them. Measured here with the other cores loaded, the loop lost 14.7% of
its IPC to a stranger on core 13. A crossing that stopped at "the rate fell 21%
and the clock fell 6.6%" would have left 15 points unexplained. It would also
have handed them to whichever factor the reader expected. The reference now
holds the sibling, and the SMT condition releases it rather than loading it.

**Second, the counters were on the core rather than on the program.** A
counter with a `cpu` target counts whatever ran on that cpu. With every other
thread busy, strangers time-share the measured core, and a core-wide count adds
their instructions to the loop's. The test therefore runs **two sessions side
by side**:

- the shipped session, which carries the core's clock and the core's cycles;
- a written session, which counts the loop's own instructions and cycles by
  pid.

Two runs, because one run observes one target (see the request below). With
the counters on the loop, the residue gains a physical meaning: the share of
the window the loop had its core. Measured under a load average of thirty, the
residue fell to **0.30** while the loop's IPC did not move. The loop lost 70%
of its core to other work, and the decomposition says so rather than calling it
"slower code".

### Measured

On a quiet machine (Ryzen 9 3900X, `amd-pstate-epp`, boost on), with the loop
counted on the core:

```
condition                     GHz    IPC    Ginstr/s
the core alone                4.37   2.33   10.17
every other core spinning     4.00   2.32    9.27     frequency: all of it
its SMT sibling spinning      4.30   1.44    6.21     frequency: 1/30 of it
```

From the test, under a load average of 21 with the other streams' suites
running, with the loop counted by pid:

```
the clock under an instruction rate, core 1 (sibling 13), load average 21.18:
  paired with its sibling   4.221 GHz  IPC 1.378   5.820 Ginstr/s  residue 1.000  (core cycles / clock 1.000)
  and 10 other cores        3.967 GHz  IPC 1.435   4.960 Ginstr/s  residue 0.871  (core cycles / clock 1.001)  | rate  -17.3 % = clock   -6.3 % + IPC   +2.9 % + residue -13.8 %
  sibling released          4.260 GHz  IPC 2.320   9.866 Ginstr/s  residue 0.998  (core cycles / clock 0.999)  | rate  +51.5 % = clock   +0.8 % + IPC  +50.9 % + residue  -0.2 %
  paired, again             4.231 GHz  IPC 1.410   5.968 Ginstr/s  residue 1.001  (core cycles / clock 1.002)  | rate   +2.5 % = clock   +0.2 % + IPC   +2.2 % + residue  +0.1 %
```

Three things are worth reading off that table:

- `core cycles / clock` is between 0.997 and 1.002 in every run. That is the
  premise of the crossing: two instruments that share nothing agree about the
  clock of a core held busy.
- Loading the other cores cost the loop 17.3% of its rate. The clock accounts
  for 6.3 points of that, and the residue for 13.8: someone else was given
  turns on core 1. The IPC stayed where it was. A frequency-only verdict would
  have been wrong by a factor of almost three.
- Releasing the sibling raised the rate by 51.5%, with 50.9 points on the IPC
  and 0.8 on the clock.

### What is asserted, and what is skipped

These are asserted in every run:

- the two instruments agree within 5% plus the frequency readings' own spread;
- the residue is not above one, because a loop cannot have had more of its
  core than the window.

The SMT half asserts that the clock moved by less than half of what the IPC
moved. It runs only when releasing the sibling moved the IPC by more than three
times the drift between the two paired runs, plus 5%.

The frequency half asserts that the IPC moved by less than half of what the
clock moved. It runs only when the clock fell by more than twice the drift plus
2%.

**Both halves skip loudly on a busy machine, and that is the honest outcome.**
Under a load average of 28–30, the reference itself ran near the all-core clock
(4.0–4.1 GHz rather than 4.3), and the two paired runs drifted by 2–4%. The
drop that loading created was then smaller than the machine's own movement.
Similarly, a sibling released under that load is taken by something else at
once. The skip message prints the load average and names both possible causes.
On a quieter machine both halves run and pass. Of the five runs of the final
version taken while writing this, with the other streams' suites sharing the
machine, the frequency half ran and passed in two and the SMT half in four.
None failed.

### Evidence: TEMPORAL, and why there is no `joins:` entry

The frequency and the counters are tied together by a core number and by
overlapping in time. Nothing propagated from one side to the other. The test
arranges for that to be sound, because the loop is pinned and never leaves its
core, but the tool cannot see the pinning.

The join engine has four match kinds, and every one of them reports `KEY` or
stronger. An `eq` on `cpu` would print `KEY` beside a correlation that is two
things happening on the same core at the same time. That would dress a
`TEMPORAL` result as a `KEY` one, which the evidence ladder exists to prevent.
So the pairing is arithmetic over two results rather than a join, the evidence
level is stated in the session, in the test and here, and a match kind whose
evidence is `TEMPORAL` is requested below.

### What the verdict cannot say: when

`BRIEF.md` asks for verdicts of the form *throttled for 1.2 s of 5 s, and the
instruction rate over that interval is not comparable to the rest*. The
frequency half of that exists: `power.freq` is a timeline, and it shows which
100 ms ticks ran below the run's own peak. The instruction half does not.
`cpu.instructions` in `count` mode is one number for the window, and `sample`
mode is refused until sampling lands with stacks. So the verdict today covers
the whole run ("this run's rate is not comparable to the reference, and this
much of the difference is the clock") and cannot be split into intervals. A
per-tick counter read is requested below.

---

## 2. Did this run throttle? Unknown, and the tool says unknown

`power.throttle` has three backends and none of them collects on this machine:

- `fs:sys` reads `thermal_throttle`, an Intel thermal-interrupt interface that
  AMD parts do not have;
- `bpf:tp` needs tracefs, which uid 1000 cannot read;
- `bpf:raw_tp` is available, but no collector exists for it.

Beside an instruction count, the whole session is refused with exit 3 and
prints no results. The test asserts exactly that: the code is 1 or 3, never 2
(the request is valid), standard output is empty, and the message names
`power.throttle`. It asserts this because the alternative is the most
expensive thing this tool could say wrongly. A result with an instruction count
and no throttle series reads exactly like a run that did not throttle.

The consequence for the verdict: **on this machine, "was this run throttled?"
is answered `unknown`, never `no`.** The effective frequency is what power and
thermal limiting look like on this part. The 6–9% all-core clock drop above is
the package's power limits at work, visible only as a lower clock with no event
behind it. That is why the frequency half of this crossing is the half of the
guardrail that is present.

A note for whoever owns `catalog/power.yaml`: the `bpf:raw_tp` backend of
`power.throttle` requires `raw_tracepoint:thermal_temperature`. That
tracepoint reports a thermal zone's temperature, not a throttling event. A
collector written against it would measure temperature and label it
throttling. The events that mean a throttle are a trip point being crossed and
a cooling device changing state. This is written under "Needs from another
stream".

---

## 3. The I/O a memory limit made

### The condition

A program writes 96 MiB of fresh random bytes to a disk-backed file, sleeps
300 ms, and reads the file back. It then prints its own `/proc/self/io`
`read_bytes`, which is the kernel's count of the bytes it caused to be fetched
from storage, and its phase boundaries in `CLOCK_MONOTONIC`. It runs twice in
a cgroup the test creates:

- once with `memory.max` at 32 MiB;
- once with it set to `max`.

The code is the same, and so are the system calls and the bytes.

The references all come from the kernel and none from the tool: the program's
`/proc/self/io`, and the cgroup's own `memory.stat` (`pgsteal_direct +
pgsteal_kswapd`) and `memory.pressure`, read before and after each run.

### Measured

```
the I/O a memory limit made -- capped at 32 MiB:
  program      wrote 96 MiB, read it back; the kernel charged it 80.4 MiB of storage reads
  cgroup       reclaimed 38122 pages by its own memory.stat, stalled 9.5 ms
  IDENTITY        79.5 MiB in 637 reads submitted by the program itself
  KEY              637 of those tied by the join to a task that was reclaiming; 38122 pages in 582 episodes reclaimed in this cgroup
  TEMPORAL       109.8 MiB written to the file's device while the program ran, 0.0 MiB of it submitted by the program; by kworker/u97:2 51.2 MiB, kworker/u97:6 32.7 MiB, ...
  device       nvme0n1: 79.6 MiB read, 109.8 MiB written over the window; the machine's memory stall 5.4 ms
  residue          0.1 MiB of the device's bytes are on no rung above
the same program, uncapped:
  program      wrote 96 MiB, read it back; the kernel charged it 0.0 MiB of storage reads
  cgroup       reclaimed 0 pages by its own memory.stat, stalled 0.0 ms
  IDENTITY         0.0 MiB in 0 reads submitted by the program itself
  KEY                0 of those tied by the join to a task that was reclaiming; 0 pages in 0 episodes reclaimed in this cgroup
  TEMPORAL         0.0 MiB written to the file's device while the program ran
  device       nvme0n1: 133.6 MiB read, 0.1 MiB written over the window; the machine's memory stall 0.0 ms
  residue        133.7 MiB of the device's bytes are on no rung above
```

Under the cap, about 190 MiB of device I/O came from the memory limit and not
from the program's code. Uncapped, the disk moved 134 MiB in the same kind of
window, and **none of it** is tied to the program on any rung. That is also a
verdict: whatever the disk did in that window, it did for somebody else. The
uncapped run's writes are not missing either. They are still dirty in the page
cache, due to the flusher thirty seconds later, which is the second half of the
lesson: the limit changed *when* the writes happened as well as whether the
reads did.

### The rungs, and what each is checked against

| Rung | What it ties | Checked against | Asserted |
|---|---|---|---|
| **IDENTITY** | reads to the program: `io.latency` reads the submitter off the block layer, and a read's submitter is the reader | the program's own `read_bytes` | within 10% (0.98–0.99 measured) |
| **KEY** | those reads to a task that was reclaiming: `mem.reclaim -> io.latency`, `eq` on `task` | the program's own reads issued after it started reading back | at least 90% matched under the program's own key (100% measured) |
| (KEY) | reclaim to the cgroup, by the `cgroup` key on `mem.reclaim`'s events | the cgroup's `memory.stat` | within −10% / +5% (exact in every run: 38122 = 38122) |
| **TEMPORAL** | writeback to the limit: the writes were submitted by kworkers during the program's run | arithmetic: to stay within 32 MiB, at least 64 of the 96 dirty MiB had to be written back before they could be dropped | at least 90% of 64 MiB on the file's device inside the program's window |
| device | `io.bytes` on the whole disk | the program's own `read_bytes` | a lower bound: at least 95% of it |

In the uncapped run, the cgroup must have no reclaim under its key, the join
must open no state under the program's key, and the program's identity reads
must be no more than the kernel charged it for. Across the two runs, the capped
identity reads must exceed the uncapped ones by at least 32 MiB.

The KEY rung says that **the thread that stalled for memory is the thread that
read from the disk.** That is evidence. It does not say the stall caused the
read, and the session says as much. The causal link is the cap, and the proof
of it is the uncapped run.

### Three things measured before they were explained

**The machine's PSI is not the cgroup's, and it is smaller.** The first
version asserted that the root's memory stall contains the cgroup's, reasoning
that a stall in a child is a stall at the root. Measured, the cgroup stalled
12.5 ms and the machine's PSI showed 6.0 ms. The root's `some` time is not a
sum over groups. It is an average over cpus, weighted by how busy each cpu
was, so a stall confined to the one cpu the program ran on is diluted by every
other busy cpu. What does hold is that a stall anywhere shows at the root as
more than zero, and that is what is asserted. The consequence belongs in any
confounder check: **on a busy machine, the system-wide `mem.pressure` shows a
container's thrashing at a fraction of its size.** The cgroup's own pressure
is the right number, and a session cannot read it beside a system-wide device
count. See the first request below.

**`mem.reclaim` stamped a window when it closed, so a join on it opened late — fixed.**
The source builds its histogram in the kernel and emits one event per bucket
per 100 ms window, timestamped at the window's end. A join state opens there
and not at the first stall, so reads issued in the same window as the first
reclaim are unmatched. The workload sleeps 300 ms between writing and reading
for this reason, and the KEY assertion counts only reads issued after reading
back began. The session's comments say so. This is a property of an aggregated
source used as the opening side of a join, and any crossing that opens on
`mem.reclaim`, `sched.runqlat` or any other bucketed source will meet it.

**What closed it**, in the eighth wave's coordinator pass: the kernel-side row
now records when its window's first episode *began*, and the event is stamped
there instead of at the drain. `ts_err_ns` stays zero on purpose — the instant
is exact and taken in this domain, which is what that field is for; what is wide
is the window, and `window_ns` beside the value already says how wide. The
workaround stays in the session because it costs nothing and the crossing's
numbers were measured with it, but a join opening on `mem.reclaim` no longer
needs it. `sched.runqlat` and the other bucketed sources still stamp at the
drain, and that is the same defect waiting in four more places.

**`io.writeback` did not reconcile with the writes it should have seen.** In
one run, `io.writeback` reported 667 pages (2.6 MiB) of flusher progress while
`io.latency` saw kworkers write 90 MiB to the same device. Its collector
already documents that `nr_pages` is a budget spent rather than a total, and
btrfs issues its writeback through its own workqueues. Whether the flusher is
simply not the path here, or the budget arithmetic undercounts, was not
settled. `io.writeback` is therefore not in the session, and nothing here
relies on it. This is written under "Needs from another stream" for `io`.

### Why everything targets the system

The limit belongs to the cgroup, but its consequence lands on the device, and
one run observes one target. A session that asks for `system` beside
`{cgroup: ...}` is refused:

```
error: one run observes one target, but this session asks for `system` and `cgroup /user.slice/...`; use one session per target
```

So every request targets the system. `mem.reclaim` events carry the cgroup as
a key and are scoped after the fact. `mem.pressure` is the machine's, with the
dilution described above.

---

## Workloads

Both are defined in `tests/cross_machine.rs`, and both are written out in the
sessions' comments so that a reader can run them by hand.

| Workload | Guarantees | Where |
|---|---|---|
| **a spin loop pinned to a core, with a known neighbourhood**: `taskset -c N sh -c 'while :; do :; done'`, plus loops on the sibling and on other cores as each condition needs | the core never idles, so its cycles over wall time are its clock; the pipeline is shared or not by construction; the package's boost budget is drawn on or not by construction | `Spinners`, `a_slowdown_...` |
| **write 96 MiB, read it back, and report**: `CHURN` in the test, run in a cgroup the test creates | identical system calls in both runs; `read_bytes` from the kernel; phase boundaries in `CLOCK_MONOTONIC`, so they can be placed against event timestamps directly | `Cgroup`, `churn_in`, `run_reclaim` |

The second one needs three things, and skips by name without any of them:

- a cgroup2 ancestor that delegates the memory controller to this user;
- a disk-backed directory, because a tmpfs file is charged to the same cgroup
  and turns a reclaim scenario into an out-of-memory kill;
- `python3`.

---

## What cannot be verified here

- **Throttling.** It cannot be created on demand, since that would mean holding
  a package at its thermal limit on a machine the suite does not own. This
  machine also cannot see it. The refusal is verified; a throttled run's
  verdict is not.
- **The frequency half on a busy machine.** When the reference already runs
  near the all-core clock, loading more cores moves the clock less than two
  identical runs drift. The test skips and prints the load average. It has
  passed on this machine at a load average of 21 and skipped at 28–30.
- **Interval verdicts.** The tool has no per-tick instruction count, so no
  statement about which part of a run is comparable can be checked.
- **Which device a stacked filesystem writes to.** On dm or md, `io.bytes`
  reports the members and not the stacked device. The device lower bound is
  then skipped with a note, and the other rungs still run.

---

## Defects and findings

1. **The frequency crossing's first reference left the SMT sibling idle**, and
   a stranger's work on it read as a 15% IPC loss. This was fixed in the test
   before anything was committed, and it is recorded here because every
   crossing that pins a core on a shared machine will meet it.
2. **A counter on a core counts strangers.** Fixed in the test with a per-pid
   session beside the core's. The general fix is for one run to carry both
   targets, which is a request.
3. **The machine's PSI dilutes a cgroup's stall** by the machine's busy cpus
   (12.5 ms read as 6.0 ms). This is not a defect in `mem.pressure`, which
   reports what the kernel publishes. It is a trap for anyone who reads the
   system-wide value as a container's. It is recorded in the session and
   above.
4. ~~**`mem.reclaim` stamps its windows at the close**~~ — **fixed**. The row
   records its window's first episode and the event is stamped there. The same
   defect is still in every other bucketed source, `sched.runqlat` first.
5. **`io.writeback` reported 2.6 MiB of flusher progress beside 90 MiB of
   kworker writes.** It is unexplained, filed for `io`, and kept out of the
   session.
6. **`power.throttle`'s eBPF backend names a temperature tracepoint.** Filed
   for `catalog/power.yaml`.

---

## Needs from another stream

Written under "Needs from another stream — stream P4" at the end of
`docs/open-findings.md`, each with the signature wanted:

- a match kind whose evidence is `TEMPORAL`, so that a pairing like "the clock
  on this core during this count" can be a `joins:` entry that says what it
  is;
- more than one target in one run, at least a machine-state source (`system`,
  `cpu`) beside a workload target (`pid`, `launch`, `cgroup`), because that is
  the shape of every confounder check;
- a per-tick read of a counting PMU event (`cpu.instructions` in
  `poll@interval` with `timeline`), without which no verdict can say *when*;
- the `power.throttle` catalog correction and the `io.writeback` question
  above.

---

## Running them

```console
make
cargo test --test cross_machine -- --nocapture --test-threads=1
omn -f examples/cross-machine-frequency.yml      # after pinning a loop to core 2
omn -f examples/cross-machine-reclaim-io.yml     # with the program beside it
```

`--nocapture` is what shows the verdict tables, and it is the only way to see
whether a half ran or was skipped. The machine-loading scenarios hold a lock
within the file, so `--test-threads=1` changes only the order of the output.
The eBPF half needs `CAP_BPF` and `CAP_PERFMON` on `target/debug/omn`, which
`make` grants and every `cargo build` drops. Without them, the reclaim
scenario skips and says so.

## `make check` on a machine shared with five other streams

Clippy and fmt are clean. The workspace suite was run three times at the end of
this stream while the other streams' suites were running on the same machine
(load averages of 10 to 30). Each run had **exactly one failure, a different
test each time, none of them in this stream's files, and each passed when
rerun alone**:

| Run | Failed | Message |
|---|---|---|
| 1 | `collect::tests::what_a_run_reports_spending_is_the_cpu_it_actually_burned` | the collector was asked to spin for 200 ms and did not |
| 2 | `collect::mem::page_faults::tests::faults_are_reported_at_the_address_that_faulted` | 65 events landed in a block of 64 pages |
| 3 | `scenario_mem::a_gibibyte_of_reads_costs_one_l1_miss_per_cache_line` | the TLB counter saw 69689 misses |

The first is already finding 38 of `docs/open-findings.md`. The second counts
faults from a process-wide collector inside a test binary whose other threads
are faulting too. The third is a TLB count on a machine whose caches are
shared. The first two are unit tests, which run first, so under `make check`
they stop the suite before any integration test runs. The third run used
`--no-fail-fast` for that reason: 1371 passed, and 1 failed.
