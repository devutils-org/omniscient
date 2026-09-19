# Stream D — stacks, and the two families that produce them

Nine defects: `cpu` 1, 2, 3, 7 and 9, `sched` 2, 3, 4 and 10. **Eight are
fixed**, each with the scenario that found it now asserting the new behaviour;
the ninth turned out to be a collision between worktrees rather than anything
in the tool, and its fix is one line in a file this stream does not own.

Two of the eight were filed against the wrong cause, and both are worth reading
for that rather than for the fix: the launched program whose stacks had no names
was not a permission problem, and the profile's fixed second was only half a
`kallsyms` parse at attach — the other half was the run *asking whether the
kernel's symbols were readable* after it had finished collecting, which is the
same parse under a question.

Every number below was measured on the development machine, with capabilities,
after `make`.

---

## Fixed

### `sched` 2 — a launched program's blocking stack had no names

The filed cause was that a launched child is non-dumpable and its
`/proc/<pid>/maps` cannot be read. It can: `collect/launch.rs` restores the
child's dumpability before it execs, and `omn` — its parent — reads the file
without trouble. The `ls -l` and `head` in the defect entry were run from a
*third* process, which Yama's `ptrace_scope` refuses; that says nothing about
the reader that matters.

The real cause is timing, and it is the trap `STATE.md` records twice already,
in a third shape. `open_target_symbols` builds the symbolizer "at the first
tick", documented as the only window in which a launched program has both
execed and not yet exited. There is no such window: `drive()` reads every
collector *before* it sleeps, so the first tick is the instant after the gate
opens, with the child inside `execve`. Instrumented, that read returned two
lines — `[vsyscall]` and nothing else.

`Symbolizer::region_of` re-reads the maps once when an address falls outside
its snapshot, which is what saved `cpu.profile` and should have saved this. It
did not, for two reasons: for a program that blocks once and then exits, the
drain carrying that block is the one in `stop`, by which time the process is
gone; and the re-read *adopted* the empty result, so the little it had was
replaced by none.

Two changes:

- **An empty read is never adopted.** `Symbolizer::refresh` keeps the mappings
  it holds when `/proc/<pid>/maps` gives it nothing, because nothing is not "a
  process with no mappings" — it is a process mid-`execve`, or a process that
  has exited. `region_of` follows the same rule and retries a bounded number of
  times rather than spending its one attempt on a read that answered nothing.
- **There is no right moment, so both collectors take them all.**
  `sched.offcpu` refreshes the target's mappings every tick and `cpu.profile`
  before each drain, at a cost of one read of a few dozen lines. The snapshot a
  stack is resolved against is then the most recent one taken while the target
  was alive.

```
$ omn sched.offcpu --emit stack -- python3 -c 'import time
time.sleep(0.3)'
  stack
    300.05ms  1 events  _start;__libc_start_main;libc.so.6+0x27781;Py_BytesMain;
                        Py_RunMain;…;clock_nanosleep
```

Asserted by `a_blocking_stack_names_the_call_that_blocked`, which now runs both
targets. The launched half had to be changed to a program that blocks **once**
and exits: the old build symbolizes a workload that sleeps every fifty
milliseconds for a second, because some drain catches it alive, so a scenario
written around that one would have passed against the defect. Verified by
running the new scenario against the pre-fix binary — it fails, three times out
of three.

There is also a unit test, `a_symbolizer_keeps_the_mappings_it_has_when_the_target_is_gone`,
which reads a live process's mappings, reaps it, and requires the snapshot to
survive the empty read that follows.

### `cpu` 7 and `sched` 3 — a profile cost a second before it sampled anything

Both are the same two causes.

1. `KernelSymbols::read` ran inside every `Symbolizer::for_process`: a parse of
   `/proc/kallsyms`, 395 318 lines here, in a debug build. It is now a
   process-wide `OnceLock`, read at most once per run and only when a kernel
   frame is actually looked up. The table describes the machine rather than any
   one process, so a copy per symbolizer was a copy of the same answer — which
   is why `sched.offcpu --system --emit stack`, one symbolizer per process that
   blocked, did not return.
2. That alone did not fix `cpu.profile`, which still cost its flat second with
   `--stack none`. `Unwinder::notes` ends with `kernel_symbols_available()`, and
   asking that question *is* the parse — so every run paid it after it had
   finished collecting, whatever it had collected. A run with no kernel frames
   cannot lose their names, so the question is now asked only by a run that
   walked into the kernel. The unwinder counts those frames as it interns them.

```
cpu.profile -d 50ms  --stack none   0.085 s     (was 1.05 s)
cpu.profile -d 400ms --stack fp     0.425 s     (was 1.41 s)
cpu.instructions -d 400ms           0.430 s     (unchanged)

sched.offcpu --emit raw   -- sleep 0.2          0.445 s   (was 0.456 s)
sched.offcpu --emit stack -- sleep 0.2          0.455 s   (was 1.096 s)
sched.offcpu --system --emit stack -d 200ms     2.3 s     (did not finish)
```

Asserted by `a_profile_costs_its_window_and_not_a_fixed_second` and
`asking_for_stacks_costs_the_stacks_and_not_the_kernels_symbol_table`. Neither
asserts a wall clock: each compares the run against the same run without the
expensive part — a counting run of the same window, and the same request
without stacks — so what is checked is the difference and not how fast this
machine is. The first takes the machine to itself, because a duration measured
beside forty-eight spinners is a measurement of the spinners.

This also closes most of finding 19 of `docs/open-findings.md`, which measured
0.4 s per process for a machine-wide symbolizing run and proposed sharing the
maps. The kernel table was the larger half and is shared now; what remains of
that finding is the per-process ELF parsing, which is real work for real
frames.

### `cpu` 1 — a profile followed one task, found nothing, and reported no loss

Not fixed by finding the work: the kernel refuses to map a ring for an inherited
per-task event, so a sampling event follows the task it was opened on and
nothing that task starts. That is a property of the mechanism, and the entry
named the honest fix — attach the degradation whenever a sampling event is
opened on a target that can be more than one task.

`cpu.profile` and `sched.switches` now do, with the measured part beside the
structural one:

```
degraded: a sampling event is not inherited, so this profile followed the target
task only: threads it starts afterwards are not sampled; it has samples from 1
task, and cpu burned in a thread or a child process the target started is in no
number here
```

A `--tid` target is exempt: one task is what was asked for, and the sentence
would be noise. `SampledEvent::sampled_threads` is read at `start` and kept,
because `stop` closes the event and the notes are gathered after it — which is
also why the existing "no unwind mode was asked for" note never reached a
reader, and now does.

It is a degradation and not a loss on purpose. The samples were never taken, so
there is no count of them; a number in `losses.dropped` would be a precision
nobody has. `a_profile_that_cannot_see_the_work_says_it_did_not_see_it` asserts
both halves — the sentence, and that `dropped` stays zero.

`mem.faults` opens the same shape of event and belongs to stream E; the
signature is under "Needs from another stream" in `docs/open-findings.md`.

### `cpu` 2 — a throttled period counted as a dropped sample

`cpu.profile` and `sched.switches` now write `self.losses.throttled +=
throttles`, which `src/collect/mem/page_faults.rs` already did and which the
comment above the `sched.switches` line already claimed. `dropped` counts only
what a ring reported losing.

`a_profile_driven_past_the_kernels_ceiling_says_what_it_did_not_take` now
asserts the field rather than only the reason — `losses.throttled` is what a
consumer filters on — and that any `dropped` count is explained by a ring
overflow, so a number of periods can never again be reported as a number of
samples.

### `cpu` 3 — `cpu.util` lost nine tenths of its readings and said nothing

`cpu.util` counts its reads and the ones that found `/proc/stat` unmoved, and
states the result. Measured rather than predicted from the interval, which is
the difference between this and `cpu.oncpu_time`'s sentence: a reader can check
it against the event count in the same run.

```
degraded: 426 of 475 reads found `/proc/stat` unmoved and produced no share,
because the kernel credits cpu time in ticks of 10 ms and a window where nothing
moved is not a window that was idle; this polls every 1 ms, so the series
carries the kernel's resolution and not the one that was asked for
```

Not counted as a loss: nothing was lost, and the readings that arrived are
exact. What was missing was the resolution the reader is actually holding.

`a_poll_faster_than_the_kernels_tick_gets_the_kernels_resolution` asserts that
the disclosure exists, names the tick, and is about the bulk of the reads rather
than a stray one. On a machine so loaded that no two reads fall inside one tick
there is no shortfall to disclose, and the scenario skips with the run's own
late-tick count as the reason.

### `cpu` 9 — a crowded pmu reported as a target that never ran

`running_ns == 0` has two causes, and `enabled_ns` tells them apart: for a
per-task event both clocks advance only while the task is on a cpu, so zero
there is a target that never got one, and non-zero is a target that ran while
its counters did not. Two messages now, and the crowded one carries the window
the target *did* run for, which is the number that settles it.

Pinned three ways, because only one of the two states can be produced on
demand:

- `a_counter_over_a_target_that_never_ran_says_that_is_what_happened` attaches
  to a process asleep for the whole window and asserts the first message, and
  that it does not blame the hardware.
- Two unit tests in `src/collect/cpu/counters.rs` drive both branches against
  readings the kernel would produce.
- `a_known_number_of_retired_instructions_is_counted_as_that_number` asserts the
  crowded message wherever this machine really does evict the ratio's group.
  That is the ground-truth one — the request beside it counted the workload in
  the same window, so the target demonstrably ran — and it is also the one that
  cannot be forced: ten `cpu.ipc` groups over one workload are time-shared at
  20 % duty on this machine rather than evicted, which is the AMD pmu having
  room to multiplex. It fires under the fan-out, which is where the defect was
  found.

### `sched` 4 — `--mode count` was `--mode trace` under another name

`count` is now what `Collector::stop`'s own doc comment says it is: the single
measurement of one window. The drains still happen, because the kernel's map has
to be emptied, but the rows are accumulated per (task, cpu) and reported once at
`stop`, over the whole window rather than over a drain. `trace` is unchanged.
The count run also says what shape it is, because "no series" is a thing a
reader plotting `rate` needs to know.

`counting_migrations_is_one_window_and_tracing_them_is_a_series` uses the
workload this family already has ground truth for — two hundred affinity
rewrites are two hundred migrations — and requires both modes to report that
same total in two different shapes: `trace` in more rows of a drain's window
each, `count` in one row per pair over a window that is the run's.

It drives the two modes from the command line rather than from
`examples/sched-migrations.yml`, because `--mode` beside `-f` is accepted and
silently discarded — that is `cpu` 5, stream A's — and a scenario that asked the
session for `count` would be handed `trace` and would check nothing. This cost
a false start: the first version of the scenario failed with "a counted row
covers 250211852 ns and a traced one 250927504 ns", which is two `trace` runs
being compared with each other.

---

## Something else

### `sched` 10 — the flaky `tests/family_sched.rs` is a collision between worktrees

Not the tool, and not either of the two causes the entry suspected.
`distinctly_named_shell` builds its workload at `std::env::temp_dir()/<name>`,
which is `/tmp/omnrunqshare`: a machine-wide path. Six worktrees of this wave
ran the same suite on this machine, each copying `/bin/sh` there and each
spinning a task answering to that `comm`, and the test identifies its own
workload with `--filter comm == 'omnrunqshare'` **machine-wide**.

Demonstrated directly, which is what rules the other causes out:

```console
$ /tmp/omnrunqshare -c 'while :; do :; done' &
$ /tmp/omnrunqshare -c 'while :; do :; done' &
$ omn sched.runqlat --system -d 700ms --emit raw --filter "comm == 'omnrunqshare'" \
    | grep -o 'tid=[0-9]*' | sort -u | wc -l
2
```

That accounts for the whole table in the defect entry — the test alone never
failed, and it failed when the machine was busiest — and it is consistent with
the per-tid `comm` cache being innocent: `sched.runqlat` reads `comm` from
`task_struct.comm` in the kernel at enqueue, so there is no userspace cache to
attribute a recycled tid to its previous occupant.

The fix is one line, in `tests/family_sched.rs`, which this stream may not edit.
It is written out under "Needs from another stream" in
`docs/open-findings.md`.

### The `region_of` re-read was load-bearing and undocumented

Worth recording because it nearly hid `sched` 2. `cpu.profile` symbolized a
launched program correctly on the old build, which the defect entry read as
`cpu.profile` doing something `sched.offcpu` should copy. It was not doing
anything: it drains every 10 ms, so its one-shot re-read almost always landed
while the target was alive. The same code in `sched.offcpu`, draining every
100 ms and often only at `stop`, landed after the target had gone. Luck rather
than method — and it is method in both now.

---

## Left

### Two tests that flake under the fan-out, in files this stream does not own

Both were seen during this work and both pass on their own, repeatedly. Neither
is caused by anything here — the second is already recorded by another stream
with the same message — but the next person to run `make check` on a loaded
machine should know what they are looking at.

- `scenario_cpu::a_profile_renders_in_every_format_that_carries_stacks` failed
  twice in about fifteen full runs with *too many* samples: `a shell spinning
  for 200 ms sampled at 997 Hz produced 1483 samples`. 1483 samples at 997 Hz is
  a second and a half of the target's cpu, so the 200 ms window overran by that
  much — the run's own loop was starved long enough that it checked its deadline
  once. `pressure_rises_when_there_are_more_runnable_tasks_than_cpus` takes the
  same shared read lock and starts forty-eight spinners, so the two do run
  together. It is a scenario that measures a rate over a window without taking
  the machine to itself, and the one-line change is written out under "Needs
  from another stream" in `docs/open-findings.md`; this stream did not make it,
  because that scenario does not name a defect this stream fixed. Its stale
  comment — the one that said each rendering costs a second of symbol reading —
  *is* this stream's, names defect 7, and has been corrected.
- `encoders::a_known_allocation_lands_on_the_chrome_trace_at_the_time_it_was_made`
  failed once, with `the 64 MiB allocation is not the step in the trace: 9932800
  before, 9932800 after`. `docs/scenarios/cpu.md` already records this one, with
  the same message and a different pair of numbers.
- `scenario_sched::an_overflowing_wakeup_ring_says_what_it_could_not_hold`
  failed once in about ten full runs and passes six times out of six on its own.
  Its own doc comment says why it is load-dependent: a contended interpreter
  lock adds wakeups the workload did not choose. Not touched.

### `mem.faults` still does not say what it did not sample

It opens a sampling event over a process exactly as `cpu.profile` and
`sched.switches` do, so it has the same blind spot and now the only one of the
three that says nothing about it. `STATE.md` claims it "states that it sees the
target task only"; it does not. It is stream E's file, and the signature is in
`docs/open-findings.md`.

### The exit codes, the late-tick reason, and the runner's pace

`cpu` 4, 5, 6, 8 and 10 and `sched` 5, 6, 8 and 9 are stream A's and stream B's
by `BRIEF.md`, and none was touched. Two of them were felt while working here
and are worth a line each for whoever does own them:

- **`--mode` beside `-f` is discarded** (the `cpu` 5 family), which cost a
  scenario a rewrite: a session run with `--mode count` on the command line
  reports a `trace` run, with nothing anywhere saying the flag was dropped.
- **The loss headline names three of the five counted fields.** A throttled run
  now correctly carries `throttled: 2` and prints `LOSS: 0 dropped, 0 failed
  reads, 0 late ticks` above the reasons that explain it. That is `io` 9,
  stream F's headline and stream A's type.

### Nothing here is unexercised

Every fix above was run on this machine, with `CAP_BPF` and `CAP_PERFMON`
granted by `make`, and every scenario named was also run against the pre-fix
binary to confirm it fails there. `make check` is clean.
