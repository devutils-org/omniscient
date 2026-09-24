# Drains that lose nothing — stream K, eleventh wave

Two tasks. The drained programs raced their own drain, and one scenario kept
failing in full runs. What follows is what landed, what each piece was checked
against, and what is left, with the reason it was left.

| | |
|---|---|
| Files | `src/bpf/map.rs` (the scheme), and the ten programs moved onto it: `runqlat.rs`, `offcpu.rs`, `proc_syscall.rs`, `sched_irq.rs`, `sched_numa.rs`, `sched_lock.rs`, `sched_futex.rs`, `migrate.rs`, `mem_alloc.rs`, `mem_stall.rs`; `offcpu.rs` and `src/collect/sched/offcpu.rs` again for the stack fix; `tests/scenario_sched.rs` |
| Tests | two scenarios, each failing against `main`: a program's own count of its calls (three runs a time), and a target's stack under a flood of other processes' stacks; ten structural unit tests (one per moved program), two in `map.rs`, two in `offcpu.rs` |
| Public methods | unchanged in every program: `drain()`, `dropped()`, `skipped()` and the rest have the signatures and meaning they had. `SyscallProgram` is S's to call and was not touched outside its maps |
| Machine | 24 cpus, kernel 7.2.6 (`CONFIG_PREEMPT=y`), `CAP_BPF` and `CAP_PERFMON` on the binary, three other streams building and testing on it |

---

## 1. The drain race

### What it was

A drained program counts in a hash map: it looks its row up and adds to it a
few instructions later. The drain walked the same map and took each row with
lookup-and-delete. A drain that took a row between a program's lookup and its
add had already copied the row out, and the add landed in a freed element --
or, the map being preallocated, in whatever key the element was handed to
next. Nothing counted it. The eighth wave measured it on the fentry histograms
(55192 of 100000 reads counted), fixed it there with a lagging two-map scheme,
and filed `runqlat` and `proc_syscall`. Every drained program in this stream's
files had the same shape.

### Measured against `main`, before anything changed

The ground truth is a program that counts its own calls: eight threads calling
`getppid` as fast as they can for 1.5 s, each counting, the sum printed at the
end (`DRAIN_FIXTURE_SOURCE` in `tests/scenario_sched.rs`). `proc.syscall` with
the filter pushed into the kernel, drained on its 100 ms tick -- fifteen drains
against rows written by eight cpus at once. The unchanged binary, five runs:

```
made 21012000 counted 21011975 short 25    counted as lost 11
made 20143000 counted 20142978 short 22    counted as lost 46
made 20395000 counted 20394980 short 20    counted as lost 266
made 20811000 counted 20810983 short 17    counted as lost 20
made 20605000 counted 20600718 short 4282  counted as lost 4300
```

**Never exact: seventeen to twenty-five calls short in the four quiet runs.**
The losses column is almost all the kernel's recursion misses (below), which
are counted machine-wide and so are an upper bound on the target's share; that
is why they cover the shortfall in four runs of five, and why the check the
scenario makes -- every call in a row or in a counted loss -- caught `main` in
the first run here and in the scenario's first run (`14 vanished without a
trace`), not in all of them. What separates the two binaries cleanly is the
runs that counted nothing lost: `main` had none, and after the change they are
exact to the call (below).

### What landed: `map::Rows`

**Two row maps, and a drain empties only the one no program can be writing.**
`fentry::HistogramProgram` found that shape. Its grace period is a whole
interval -- the map drained is the one retired a tick ago -- which costs every
row a tick of lateness and makes the last drain a special case, with a 1 ms
sleep standing for "every program has finished". Neither fits here: the
collectors stamp each drain's window from the drain's own clock, and S owns the
`proc` ones, so a drain that returned the previous interval's rows would have
put every `proc.syscall` event in the wrong window without anyone being able to
fix it.

So `Rows` keeps the scheme and makes the grace period exact:

- **The word is a one-slot `BPF_MAP_TYPE_ARRAY_OF_MAPS`.** A program looks the
  row map up through it, once per run, keeps the pointer on its stack
  (`select_rows`) and reads it back for the row lookup and the row insert
  (`row_map`). Once, because two reads of the slot in one run can straddle a
  swap and find a row in one map and create it in the other.
- **A drain swaps the slot, waits for a grace period, and empties the map it
  retired.** Every program here runs inside an RCU read-side section
  (`__bpf_trace_run` and the trampolines take `rcu_read_lock()` around it), so
  once `synchronize_rcu()` returns, no program that read the old slot is still
  running. The retired map is taken whole: nothing a tick late, nothing left
  at the end of the run, and `drain()` means what it meant.

**The kernel's own swap is not the grace period, and this is the finding worth
keeping.** `maybe_wait_bpf_programs` promises one on a map-of-maps update, in a
comment that is exactly this contract. Measured here, the update returns in
**20 to 30 us** (a plain array update: 2 us), and a real `synchronize_rcu()` --
`membarrier(MEMBARRIER_CMD_GLOBAL)` is that syscall from userspace -- takes
**5 to 18 ms**. Whatever the update waits for on 7.2, it is not every running
program, so `Rows::drain` asks for the grace period explicitly. A kernel that
withdraws `MEMBARRIER_CMD_GLOBAL` (it does under `nohz_full`) is refused when
the program loads, with that reason, rather than drained unsafely.

**And "a program runs for microseconds" is not a bound on this kernel.** The
fixture's `sys_exit` program -- a hook no interrupt enters -- is skipped by the
kernel's recursion guard thousands of times in some runs (4300 and 15829
above and below), which only happens if a run of the program was preempted on
that cpu and another thread's syscall reached it there. Raw-tracepoint programs
are preemptible here. That is why the grace period is the kernel's and not a
sleep, and why `fentry`'s 1 ms is filed below.

### After, same fixture, five runs

```
made 20263000 counted 20258201 short 4799  counted as lost 4999
made 21305000 counted 21305000 short 0     counted as lost 0
made 21309000 counted 21309000 short 0     counted as lost 0
made 21331000 counted 21331000 short 0     counted as lost 2
made 20872000 counted 20856171 short 15829 counted as lost 15829
```

**Exact to the call in every run the kernel skipped nothing**, and never
shorter than what was counted as lost. The scenario asserts both sides on
every run: no more counted than made (an add landing in another row's element
would over-count), and made no more than counted plus counted losses.

`every_call_a_program_made_is_in_a_row_or_a_counted_loss_on_a_short_tick`, in
`tests/scenario_sched.rs`, three runs each time it runs.

### What it costs

The grace period is paid once per drain, in the runner's thread, as sleep and
not as cpu: 5 to 18 ms here against a 100 ms tick. One drained source costs
nothing a reader would see. A session with six or more drained sources, drained
one after another in one round, can push a round past its tick, and the runner
counts that as `late_ticks` with its reason, as it counts any other overrun.
One grace period shared by every source of a round would remove that; it needs
the runner to drain in two phases (swap all, wait once, take all), which is a
change to `src/collect/mod.rs` and is filed below.

### The programs, and the one left as it was

| Program | Row map | Moved |
|---|---|---|
| `proc_syscall` | `(task, syscall, bucket)` | yes |
| `runqlat` | `(task, cpu, prio, bucket)` | yes |
| `offcpu` | the totals map | yes |
| `sched_irq`, `sched_lock`, `sched_futex`, `sched_numa` | rows | yes |
| `migrate` | rows | yes |
| `mem_alloc`, `mem_stall` | rows | yes |
| `sched_drm` | the `done` map | **no**: it only inserts a finished job under a key no other run writes, and never reads a row to add to it, so a drain that takes a row cannot race an addition to it |

Every moved program has a test that its row map is taken from the slot once
and used by both the lookup and the insert
(`the_row_map_is_taken_from_the_slot_once_per_run`, through
`map::assert_rows_taken_once`, which has its own test that a program reading
the slot twice is refused). Every one already had a stack-region test and a
byte-coverage test (finding 46); the new stack slot is in each region list, so
those tests would refuse it overlapping anything.

---

## 2. `a_blocking_stack_names_the_call_that_blocked`

**The machine, and a defect in the tool that only a loaded machine shows.
Not the symbolizer, and not the exit.** Fixed in the program.

### What it was

`sched.offcpu`'s `sched_switch` program called `bpf_get_stackid` for every
task on the machine that blocked, and compared the thread group with the
target's only afterwards. The stack map has 4096 buckets, holds one stack per
bucket, and answers `-EEXIST` for a second stack in a bucket rather than
replacing the first (no `BPF_F_REUSE_STACKID`, on purpose: replacing it would
make an id already handed out name another stack). So every blocking stack of
every process on the machine competed for 4096 buckets with the target's,
and on a machine running a full suite and three other agents' builds the map
was full within a few hundred milliseconds of the attach. Whether the test
passed depended on whether the Python target blocked before the rest of the
machine filled the map: alone it always did, in a full run it often did not.

And the loss was misreported. The errno was dropped in the decode, and every
failed stack was counted as "had no user stack to walk: a kernel thread has
none, and a task built without frame pointers may have none" -- a reason that
sends a reader to rebuild a program that was fine.

### How it was found

Neither load alone reproduced it: ten runs under 30 cpu hogs on 24 cpus, and
twelve beside five other scenario files, all passed. The symbolizer's
per-tick refresh was read and holds. What the failing shape needs is many
*distinct blocking stacks* elsewhere on the machine, so the test now makes
them: `STACK_NOISE_SOURCE`, eight threads blocking at the end of a random
twenty-deep walk through two functions -- tens of thousands of distinct
stacks a second. With it running, the unchanged binary's output:

```
degraded: 10 blocks had no user stack to walk: a kernel thread has none, and a
task built without frame pointers may have none the kernel can follow
```

for a Python whose ten sleeps all had a perfectly walkable stack.

### What landed

- **The target is compared before the stack is walked**, so a narrowed run
  asks the kernel for the target's stacks and nobody else's. It is also
  cheaper: the walk was being paid for every blocking task on the machine to
  keep one process's.
- **The errno is kept** (`OffCpu::stack_error`), and a stack the map had no
  room for is its own note: "N blocks were walked and their stack not kept:
  the kernel's stack map holds one stack per bucket over 4096 buckets and
  refuses a second". A machine-wide run under the same noise says it for
  155186 blocks, which is true, and no longer blames frame pointers.

### The scenarios

| Test | Against `main` | Now |
|---|---|---|
| `a_targets_blocking_stack_is_named_while_another_process_floods_the_stack_map` | fails 5 of 5, all ten blocks without a stack | passes 3 of 3 |
| `a_blocking_stack_names_the_call_that_blocked`, unchanged | passes alone, failed 2 of 3 full runs | passes; one full run clean |
| `bpf::offcpu::tests::a_narrowed_run_walks_only_the_targets_stacks` | -- | the comparison precedes the walk |
| `bpf::offcpu::tests::a_failed_stack_keeps_the_kernels_reason` | -- | `EEXIST` and `EFAULT` kept apart |

The old test's assertion was left as it is: with the stacks the target does
not own out of its way, what it asserts -- that the sleep is *there* -- is
something the tool can promise.

### What is left

**A machine-wide run still has 4096 buckets for the whole machine.** That is
now said, with the count, rather than hidden; making it bigger moves the
threshold without removing it. The per-process answer is the fix above; the
machine-wide one would be a map sized to the run or stacks carried per event,
and neither is needed by anything asked of this wave.

---

## Needs from another stream

Filed under "the eleventh wave, stream K" in `docs/open-findings.md`.

## The suite

Two full runs (`make check` and one `--no-fail-fast` after each red), and
every one of this stream's tests green in both. What was red, each rerun alone
once:

| Test | Seen | Alone | Why it is not this stream's |
|---|---|---|---|
| `scenario_mem::every_mode_and_emit_of_the_sampled_sources_is_served_or_refused_by_the_right_code` | both runs | failed once, passed once | `mem.latency -e stack` exits 1 one run in ten **on `main` too** (1 of 10 with the unchanged binary); filed |
| `scenario_sched::a_workload_that_is_half_blocked_measures_as_half_blocked` | once | passed | its `sched.wakeup` half (a ring, not a drain) saw 1584 wakeups for 1200 +-25 %; the drained off-cpu half passed |
| `scenario_mem::both_huge_page_backends_read_the_same_promotion_counter` | while iterating | failed | finding 43's tally; a machine-wide THP counter |
| `collect::tests::weighing_a_run_costs_a_fraction_of_the_run`, `scenario_net::every_segment_the_kernel_resent_is_one_this_run_saw`, `scenario_proc::what_the_signal_ring_could_not_hold_is_counted_and_closes_against_what_was_sent`, `join_engine::the_resident_size_decomposes_into_live_and_retained_with_the_residue_the_program_predicts` | once each | passed | none reads a drained program; filed for the tally |

The session-index test was green: this stream added no session.

**For the coordinator: `cargo fmt --check` is red on `main`**, and so is the
last step of `make check`, for one assertion in `tests/scenario_runtime.rs`
(line 926, from `a883bea`) that rustfmt wraps. Not this stream's file, so it
is left as it is here; `cargo fmt` on `main` fixes it.

## Index rows

None: this stream added no session.

## Ignored tests made ready

None.
