# The twenty-fourth wave, stream sched

Branch `feat/w24-sched`. Defects first, then the comparison's gaps
(`docs/fixes/w22-*.md`, `docs/fixes/w23-*.md`). No shared layer touched:
`src/bpf/mod.rs`, `insn.rs`, `map.rs`, `ring.rs`, `perf.rs`,
`src/collect/mod.rs` and `src/catalog.rs` are as main has them.

## What landed

| Commit | What |
|---|---|
| `405d015` | `fix(sched)`: **defect, bpftrace 2** -- `sched.irq --filter 'kind == "hard"'` selects the hard rows (the filter layer read `kind` as the source's own kind). **Defect, sysstat 1** -- a counted `sched.switches` rate on a task is per second of the run, not per second the task was on a cpu. |
| `31cc433` | `feat(sched)`: `sched.runqlat` carries the run's summary over `wake_to_run_ns` (perfsched 8): `fields` `wake_to_run_mean_ns`, `wait_mean_ns`, `waits` and both sums, and a sentence. |
| `09a5679` | `feat(sched)`: **`sched.interrupts`** (interrupt counts per cpu, line and softirq vector, sysstat 8, bpftrace 5) and **`sched.runqueue`** (`procs_running`, `procs_blocked`, load averages, sysstat 5). |
| `605ea38` | `feat(sched)`: **`ppid`** on `sched.offcpu`, `sched.runqlat` and `sched.futex` rows. The descendants map's value is now the parent; the free program keeps a gone child's parent for one drain. |
| `370b293` | `feat(sched)`: **`sched.slices`**, one event per time slice (perfsched 9, `perf sched timehist`'s line): `run_ns`, `queue_ns`, `off_ns`, `state`, `next_comm`, `ppid`. |
| `247e31a` | `feat(sched)`: **`sched.lock` by kind** (perfsched 6, half): `kind` is a filter field, so `topk:N@kind` and `summary@kind` (stream emit's `@<attr>`) group the waits as `perf lock contention` groups by type. |

The predicate on `kind` is asked of the row's own kind by
`collect::sched::matches_as`, used by `sched.irq` and `sched.lock`. The
per-switch program lives in `src/bpf/switch_count.rs`, reached through
`SwitchCountProgram::load_stream`, so that no line of `src/bpf/mod.rs` had
to change.

| `f8833d3` | `fix(sched)`: **review 1 and 2** -- `sched.slices`' `off_ns` is the gap less the queue wait, and the wait adds what `run_delay` grew by while the task was away; a switch in with no stamp is flagged, never a zero. |
| `dc2d66f` | `fix(sched)`: **review 3** -- `sched.interrupts` counts a counter that wrapped past 2^32, and a reset or a failed closing read as a loss. |

## Shown vs reasoned

All shown on this machine (24 cpus, 7.2.8, fifteen other streams running),
against something `omn` did not compute.

- **irq filter**: 2 s machine-wide, `kind == "hard"`: 400 handler runs,
  against `/proc/interrupts`' 428 over the run and its start-up; `"soft"`:
  10 610 against `/proc/softirqs`' 12 236. Before: 0 series.
- **switches rate**: `sched.switches -m count -e rate -- switch.py 1000`:
  1002 over 1.07 s, **940/s** (`pidstat -w`'s shape: about 945/s). Before:
  1010 over 30.39 ms "the counter was enabled", 33 235/s.
- **runqlat summary**: `sleeper.py 100 2`: 106 waits, `wake_to_run_mean_ns`
  3.6 us; a bpftrace script timing `sched_wakeup` to `sched_switch` for the
  same workload: 100 waits, 4.2 us. `perf sched latency` said 119 us average
  on a third run under its own recording; I take that as perf's recording
  cost, not as either number's error (reasoned).
- **sched.interrupts**: three 1 s polls, 17 585, 24 981 and 40 738 hard
  interrupts, against vmstat's `in` 17 850, 22 806 and 42 021 over its own
  seconds (the windows are not aligned). The first poll's interval opens at
  the first read: three full 1 s windows, no leading few-ms event.
- **sched.runqueue**: `running` 8 to 19 and `blocked` 2, against vmstat's `r`
  4 to 23 and `b` 2 over the same seconds; four spinners read as at least 5
  (test).
- **ppid**: `sh -c` running five `sh -c 'sleep; sleep'` children: every row of
  all eleven processes, 20 ms lives each, carried its parent, and the tree is
  the script's (root under the launcher, five shells under the root, one
  `sleep` under each).
- **sched.slices**: `sleeper.py 100 2`: 102 slices, runs adding up to
  11.95 ms against the reap's `cpu_ns` 11.62 ms; 100 `sleeping` slices,
  `off_ns` median 2.05 ms, `perf sched timehist`'s wait column 2.047 to
  2.051 ms. 64 threads spinning 1 s of wall time: runs 19.23 s against
  19.11 s; runs plus queue waits 63.1 s for 64 runnable seconds.
- **sched.slices' `queue_ns` on a wake from idle is 0**, where timehist's
  `sch delay` is 3 to 11 us: the field is the kernel's `rq->clock -
  last_queued`, as `sched.runqlat`'s `total_ns` and `schedstat` count it, and
  the scheduler does not move the clock between a wakeup onto an idle cpu and
  the switch. Under contention it is non-zero on 8842 of 8947 slices. Which
  of the two a reader wants is said in the caveat; the tracepoints' figure
  is not in this source (not done, 3).
- **sched.lock by kind**: `contend.py 8 2`, `topk:5@kind`: `write` first;
  1706 and 1748 waits (176 and 192 ms) in two runs; `perf lock contention -b`
  on a third run: the workload's `rwsem:W` callers 2736 waits, 341 ms. The
  order agrees; the totals differ by run, and this source's caveat says the
  program on the switch changes the convoy (reasoned, not held to a
  tolerance).

## Tests

- `cargo test --lib collect::sched`: 81 passed. `cargo test --lib bpf::`:
  404 passed (the sched programs' and every family's; run once because
  `sched_common.rs` is used by `cpu`'s `oncpu.rs`).
  `cargo test --lib catalog`: 61 passed.
- `cargo test --test family_sched`: 25 passed (8 new: the counted switch
  rate, the irq half, the runqlat fields, the run queue, the interrupt
  counts, `ppid`, the slices, the lock kind), after `make`, privileged.
- `cargo test --test scenario_sched`: 55 passed.
- `cargo test --test cli`: 33 passed (the catalog grew three entries).
- `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt
  --check`: clean.

## Not done, and why (ranked)

1. **A per-cpu `sched.runqlen`** (bpftrace 5, sampled run queue length):
   `/proc` does not export a cpu's `nr_running`, so it needs a
   `BPF_PROG_TYPE_PERF_EVENT` program on a 99 Hz cpu-clock event per cpu
   reading `bpf_this_cpu_ptr(&runqueues)->nr_running` (the `pcpu_hot` trick
   `wakeup.rs` uses), bucketed in a per-cpu array. A new program file needs a
   `mod` line in `src/bpf/mod.rs`, a shared layer; `sched.runqueue` is the
   machine's sum meanwhile.
2. **`sched.lock`'s caller and address** (perfsched 6, the other half).
   `perf lock contention`'s caller is a kernel frame, and this source walks
   user stacks only (kernel frames need `/proc/kallsyms` as root). `-l` (by
   lock address) needs the address in the row's key in `sched_lock.rs`,
   which widens the map's cardinality. Not started.
3. **The tracepoints' queue wait in `sched.slices`** (timehist's `sch
   delay`): needs a `sched_wakeup` stamp per task in the per-switch program,
   as `sched.runqlat` keeps one.
4. **`ppid` as a filter field** (`topk@ppid`), and on `sched.switches`,
   `sched.migrations`, `sched.wakeup` and `sched.lock` rows. The programs
   give the parents (`parents()` exists on each with descendants); the
   collectors do not attach them yet, and the filter-field tests would need
   to allow an attribute attached per drain.
5. **The `process` suspect reading sleep, not lock** (`docs/fixes/w20-process.md`):
   the cause is not in the sched collectors. `sched.offcpu` reports a futex
   wait and a `nanosleep` as the same kernel state (`sleeping`), because the
   kernel puts both in `TASK_INTERRUPTIBLE`; `sched.futex` holds the 0.25 s
   on the lock, by task. The timebar would have to subtract `sched.futex`'s
   waits from the task's `sleeping` time (views/top).

## Privileged runs wanted

None beyond what `sudo -n` grants. The references used above, to rerun:

```console
cd ../omniscient-w24-sched && make
W=tests/reference/workloads/perfsched
d=$(mktemp); sudo -n perf sched record -q -o $d -- python3 $W/sleeper.py 100 2
sudo -n perf sched timehist -i $d | grep omnref-sleep | head
sudo -n perf lock contention -b -- python3 $W/contend.py 8 2
sudo -n bpftrace -e 'tracepoint:sched:sched_wakeup /args.comm == "omnref-sleep"/ { @ts[args.pid] = nsecs; }
  tracepoint:sched:sched_switch /args.next_comm == "omnref-sleep" && @ts[args.next_pid]/
  { @lat = stats(nsecs - @ts[args.next_pid]); delete(@ts[args.next_pid]); }' -c "python3 $W/sleeper.py 100 2"
```

## Needs from another stream

- **emit** (`src/filter.rs`): read an attribute named `kind` before the
  event's own kind, or refuse `kind` as a filter field on a source whose rows
  carry one. That is the root of the irq defect; the sched collectors work
  around it where they apply the filter themselves, and `sched.interrupts`
  (filtered by the runner) does not declare `kind` until it lands.
- **cpu** (`src/collect/cpu/counters.rs`): `cpu.context_switches -m count -e
  rate` on a task divides by `enabled_ns`, the task's time on a cpu, the
  defect fixed here for `sched.switches` (wrapped in
  `collect::sched::WallWindow`). Any per-task software counter's rate has
  the same denominator.
- **reference**: the `pidstat-w` pair's `known_defect` now fails its marker
  (940/s against about 945/s); remove it. New pairs this makes possible:
  vmstat `in` against `sched.interrupts`, vmstat `r`/`b` and `uptime`
  against `sched.runqueue`, `perf sched timehist` (run, wait) against
  `sched.slices`, `runqlat.bt`'s mean against `fields.wake_to_run_mean_ns`,
  `perf lock contention`'s type against `sched.lock -e topk@kind`.
- **triage**: `ppid` is on `sched.offcpu`, `sched.runqlat` and `sched.futex`
  rows (the job restricted to the followed child's parent,
  `docs/open-findings.md`, "To the sched sources ... `ppid`").
- **views / top**: the `process` suspect (not done, 5).
- **coordinator**: `CATALOG.md` and `man/omn.1` for `sched.interrupts`,
  `sched.runqueue`, `sched.slices`, and `sched.lock`'s `kind` filter field.

## Review

1. **`off_ns` held the queue wait** (defect): right. `off_ns` is now
   `in - left - queue_ns`, saturating; the unit test reads 3 700 where it
   read 4 000, and the facts and caveat say the two add up to the gap.
2. **A missing queue stamp read as `queue_ns = 0`** (defect): right that it
   was uncounted, but on this machine the zeros were mostly not missing
   stamps. With the stamp flagged in the record (`matched`'s bit 4), 48
   spinning threads on 24 cpus gave no switch in without a stamp; the 73
   zero waits among 7 241 preempted slices, and the ms of `off_ns` beside
   them, were tasks the balancer moved while runnable: the first cpu's
   `sched_info_dequeue` closes that part of the wait into `run_delay` and
   clears the stamp, and the second cpu's stamp is the rest only (what
   `sched_common::push_run_delay` already says of `sched.runqlat`). The
   record now carries both ends' `run_delay` and the wait adds its growth
   since the switch out: 5 zeros left, `off_ns` 3.3 ms against 0.52 s. A
   switch in with neither a stamp nor a switch out to measure from carries
   no `queue_ns` or `off_ns` and is counted in the facts. The 5 left are
   slices handed to `migration/N`, the stopper's move, not chased.
3. **`sched.interrupts` dropped a backwards counter** (gap): right, the
   kernel's counts are `unsigned int`. A count that went backwards from the
   upper half of 2^32 is counted across the wrap, and the wraps are a fact;
   any other backwards count resets (a line freed and taken again under its
   name), counts from zero, and is a failed read with a note. The failed
   closing read in count mode is a failed read with a note too. Not shown
   live: no counter here is near 2^32; the unit test holds it.

Tests of the round: `cargo test --lib sched` 141 passed, `cargo test --lib
catalog` 66, `cargo test --test family_sched -- slices interrupt` 4 (after
`make`, privileged; `time_slices_*` three times), clippy and fmt clean.

## Decision lines for `DECISIONS.md`

- A counted `sched.switches` rate on a task is per second of the run; the
  counter's enabled time, which advances only while the task runs, is kept
  as `oncpu_ns` and is not the denominator.
- On a sched source, a filter field named `kind` is the row's kind (an
  interrupt's half, a lock's type), not the source's.
- `sched.slices`' `queue_ns`, like `sched.runqlat`'s value, is the kernel's
  `sched_info` account; the tracepoints' wakeup-to-switch figure is a
  different quantity and, where it is carried, has its own name
  (`wake_to_run_ns`).
- `sched.slices`' `off_ns` and `queue_ns` add up to the gap between a
  switch out and the next switch in; the wait counts the part the kernel
  closed into `run_delay` on a cpu the task was moved from.
