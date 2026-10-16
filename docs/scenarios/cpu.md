# Scenarios — `cpu`

What is worth measuring in this family, and how you know the answer is right.

Fourteen sources, thirteen of them with a collector, and every one of them
answers with a number that is plausible whatever it does. A counter that is
wrong by a factor of two looks exactly like a counter that is right; a profile
that missed every thread but one renders as a flame graph. So nothing below is
checked against what the tool computed. Each scenario starts from a workload
with a property chosen in `tests/scenario_cpu.rs` — a thousand million
instructions retired, three hundred milliseconds of cpu burned, fifty sleeps, a
core with one tenant — and asks whether the tool found it.

The sessions are in `examples/cpu-*.yml`, the checks in
`tests/scenario_cpu.rs`, and the defects they found are at the end of this
document. There are ten, and three of them are the kind this wave exists to
find: a run that lost everything and said it lost nothing.

---

## The scenarios, by the question they answer

### Did the counter count what the program actually did?

`examples/cpu-known-work.yml` ·
`a_known_number_of_retired_instructions_is_counted_as_that_number`

The workload is a loop of `dec` and `jnz`: two retired instructions per turn and
nothing else, so five hundred million turns is a thousand million instructions.
The number is in `retire()` in the test file, which is what makes it ground
truth rather than a second opinion.

Becoming the workload — a shell, an `exec`, a dynamic linker, a test harness
choosing a test — costs instructions too, so the scenario measures that constant
instead of assuming it: the same session runs with a loop of one turn, and the
difference between the two runs is the loop. On this machine the constant is
about **seven million instructions**, under a hundredth of what is being
counted, so the absolute number is checked as well as the difference:

```
expected                1 000 000 000
one turn (the constant)     7 075 425
five hundred million    1 008 607 400   (+0.86 % outright)
the difference          1 001 531 975   (+0.15 % of what the loop retires)
```

The session runs `cpu.ipc` over the same workload in the same window, and its
`instructions` term — read inside a counter group beside `cycles` — has to be
the same count as the one read on its own. Two roads, one workload, and the
workload is the arbiter.

The example takes its workload from `$OMN_SCENARIO_WORKLOAD` when that is set,
so a reader can point it at their own known loop; without it the workload is a
shell loop, which shows the shape of the answer and is not ground truth. The
file says so.

The scenario also knows when it cannot check anything, and this is the tool
telling it. When more counter groups are open on a machine than the pmu has,
the kernel time-shares them and scales what it returns — an extrapolation from
the slices the group was on for, which the run reports as a degradation. An
extrapolation is not a count, so the scenario reads that degradation and stops,
naming it. It fires on this machine when another worktree of this wave is
running its own counter tests, and it is why the scenario is not flaky: the
first version of it failed once with an `instructions` term 58 % below the
workload's own number, which was a scaled value being checked as a counted one.

### Where did the time go — on a cpu, or waiting?

`examples/cpu-where-the-time-went.yml` ·
`the_samples_of_a_profile_count_the_cpu_time_and_not_the_wall_clock`

A program that takes a second may have used a second of cpu or fifty
milliseconds and waited for the rest. `cpu.oncpu_time` and `cpu.profile` answer
that together, and the scenario checks them against each other because each of
them alone is only believable.

The claim under test is in `src/collect/cpu/profile.rs`: the sampling clock
counts **cpu** time, so 997 Hz is one sample per 1.003 ms of cpu, not per
1.003 ms of wall clock. The workload spends about half its life computing and
half asleep, so the prediction is arithmetic — samples ≈ 997 × the cpu seconds
the other source measured — and a profile that sampled the sleep would come back
with twice as many.

Both requests carry `keys: [task]`. This is the pair phase two's P2 starts from.

### How much cpu did this program really get?

`counted_oncpu_time_is_task_clock_and_agrees_with_the_reap` ·
`task_clock_counts_the_cpu_time_the_thread_ran`

`cpu.oncpu_time -m count` is `perf stat`'s task-clock, through `pmu:sw`: one
total over the program and the children it starts. It agrees with the
thread's own `CLOCK_THREAD_CPUTIME_ID` to under 1 % of a 200 ms spin, and with
the reap's `cpu_ns` in the same run to 0.4 to 3 % (716.6 against 696.8 ms,
485.8 against 483.8 ms, 390.0 against 382.7 ms with two children).

`a_sleeping_program_consumes_no_cpu_and_a_burning_one_consumes_what_it_burned` ·
`a_program_that_blocks_after_working_is_credited_only_with_the_work`

Two workloads, one source, one emit: a program that sleeps for 300 ms and a
program that burns 300 ms of cpu. `cpu.oncpu_time` has to report almost nothing
for the first and the 300 ms for the second.

The property is an **amount of cpu, not a share of the window**, and that is
deliberate. This machine runs eight streams' suites at once; a workload that
spun for 300 ms of wall clock would consume whatever was left over, and a
scenario whose ground truth moved with the load would be measuring the machine.
So `burn()` spins until `CLOCK_THREAD_CPUTIME_ID` has advanced by the amount
this file chose, and the check does not move when the machine is busy.

The third workload, `split`, burns and then blocks: the tool must credit it with
what it burned, and the window must be long enough to hold the sleep as well —
otherwise the workload never blocked and the scenario proved nothing.

### Is this core busy, and is it busy with my work?

`examples/cpu-one-busy-core.yml` ·
`a_core_with_one_tenant_reads_the_same_through_two_files`

Twenty-four cores with one saturated average four per cent, which is
indistinguishable from twenty-four cores doing almost nothing. So the question
is asked of a core: `cpu.util --cpu N` keeps the lines of `/proc/stat` apart
rather than averaging them into a number that describes no core.

Pin a spinning task to a core and the answer is known before the run: that core
is busy for the whole window, and the share has to be `1.000`. It is, exactly.

The same scenario is the closest thing this family has to a backend comparison.
`cpu.util --cpu N` reads `/proc/stat` and `cpu.oncpu_time --pid` reads
`/proc/<pid>/stat` — different files, different collectors, different keys — and
a core whose only tenant is the scenario's own spinner makes them the same
number. Where this machine is busy with seven other streams and something else
lands on that core, only the inequality can be checked: **a core is at least as
busy as the tasks pinned to it**, which a `busy` computed over the wrong
denominator or read off the wrong line would break. The test says which of the
two it was able to check.

It takes two runs rather than one session, because a session observes one target
and these are two:

```console
$ omn -f two-targets.yml
error: one run observes one target, but this session asks for `cpu 23` and
       `pid 3507697`; use one session per target
```

Worth knowing before phase two tries to join a per-core series to a per-process
one.

### What was the machine doing, state by state?

`the_windows_split_is_the_ticks_between_the_first_read_and_the_last`

`top`'s and `mpstat`'s `us`/`sy`/`wa`/`st` over the run: `cpu.util` puts the
window's ticks per state in the results' `fields` (`user_ticks`,
`system_ticks`, `iowait_ticks`, `steal_ticks`, ..., `ticks`; prefixed
`cpu<N>.` per core) and the shares in a note, from the baseline and the last
read -- exact, where a mean of per-tick shares weighs a short tick like a long
one. Alongside `mpstat 4 1` over the same four seconds: user 32.5 % against
32.90, system 9.0 against 9.27, iowait 7.4 against 7.39; and 41.5 / 5.3 / 7.1
against 41.39 / 5.28 / 7.06.

### Is the machine busy, or oversubscribed?

`examples/cpu-saturation.yml` ·
`pressure_rises_when_there_are_more_runnable_tasks_than_cpus`

Different questions with the same wrong answer, and `cpu.util` and
`cpu.pressure` separate them. The ground truth is the scheduler's own
arithmetic rather than anything the tool computed: twice as many spinning tasks
as there are cpus guarantees that about half of them are runnable and not
running at every instant, and PSI counts exactly that time.

The check is two-sided, because a counter stuck at a plausible value passes a
one-sided one: the same measurement on the machine *before* the load starts has
to be far lower. On an idle machine that is a stall share of 0.000 against
0.816 under load. Where the machine was already stalled more than half the
window before this scenario loaded it — which happens when seven other streams
are running their suites — it says so and stops, because its own load cannot
then be told from somebody else's.

### Where does a counter's event happen?

`a_counting_source_samples_on_its_own_counter` ·
`a_cycles_profile_samples_one_period_of_cycles_at_a_time` ·
`branches_match_a_loop_with_one_branch_per_iteration`

Every counting source of the family samples on its own counter: `cpu.cycles
-m sample@997hz -e stack` is `perf record`'s default, and `cpu.branch_misses`
sampled is `perf record -e branch-misses`. Cycles sample at a fixed period,
the cpu's highest frequency over the rate, so the samples times the period are
the cycles a counter beside them counted (held to 10 %); the others sample in
the kernel's frequency mode, each sample's `count` the period the kernel
chose for it (`PERF_SAMPLE_PERIOD`), so their shares are perf's and their sum
estimates the count: `cpu.instructions -m sample@997hz` summed 6.186 G where
`-m count` read 6.191 G, and `mem.cache_l1`, sampled on its misses since the
twenty-fifth wave, 40.33 M against 40.32 M. `--keys insn` names each sample's
leaf instruction, `hot+0x5a`, at the instruction boundaries `objdump -d`
prints. On the
perfrecord `hot` workload a cycles profile read `hot_beta` 0.495, `hot_gamma`
0.291, `hot_alpha` 0.200 against `perf record -e cycles`' 0.483, 0.306, 0.206.

`cpu.branches` counts retired branches, against a loop that retires one per
iteration (`known_loop`), within 1 %; through the binary 250.42 M on a C loop
of 2.5 branches per iteration over 1e8, where `perf stat` read 250.47 M.

### How many times was the program taken off the cpu?

`a_workload_that_sleeps_a_chosen_number_of_times_is_switched_that_many_times`

A sleep is exactly one switch away from the task, so a workload that sleeps
fifty times guarantees fifty switches. The check is a floor and a ceiling rather
than an equality: the floor is the fifty the workload chose, and the ceiling is
measured the same way the instruction constant is, by running the same program
with no sleeps at all.

### Was this run throttled, and is it therefore worth keeping?

`examples/cpu-throttled-cgroup.yml` ·
`a_cgroup_capped_at_a_tenth_of_a_cpu_is_stopped_for_nine_tenths_of_the_time`

CFS hands out a quota once per period: a task that wants a whole cpu and may
have a tenth of one is stopped for about ninety milliseconds of every hundred,
so `throttled_ns` has to be about nine tenths of the window. Nothing about that
prediction comes from the tool.

**This machine cannot serve it.** The cpu controller is not delegated to a user
session's cgroup, so no cgroup that can be created here can carry a quota:

```
skipped: the cpu controller is not delegated to
`/user.slice/user-1000.slice/user@1000.service/app.slice/tmux-spawn-….scope`
(it offers `memory pids`), so no cgroup made here can carry a quota and nothing
can be throttled
```

The scenario and the session are here so that a machine which *can* serve them
runs them. The example's header carries the four commands that set the cgroup
up.

### Does every axis the catalog declares work?

`every_mode_and_emit_the_catalog_declares_is_served_or_says_what_is_missing` ·
`a_counted_window_is_the_same_number_in_every_format_that_carries_it` ·
`a_profile_renders_in_every_format_that_carries_stacks`

The sweep is a projection of the catalog, the way the CLI is: it reads
`omn list cpu --format json` and runs every (source, mode, emit) the catalog
declares, so a source, a mode or an emit added to `catalog/cpu.yaml` is swept
the next time it runs with nothing here to keep up to date. **93 combinations,
and 35 of the 53 (source, emit) pairs are reachable on this machine.**

The eighteen that are not are listed below, each with the mechanism it waits
on, and the test fails if the list grows. That is the point of the class: `omn
list cpu.cycles` says the source emits `stack`, and it does — in `sample` mode,
which no counting source implements. A promise the catalog makes and the build
does not keep is invisible in the one place people look.

| Source | Emit | Reachable only in | Waiting on |
|---|---|---|---|
| `cpu.cycles`, `cpu.instructions` | `hist`, `topk`, `stack` | `sample` | a sampling collector for counting sources |
| `cpu.branch_misses`, `cpu.cache_misses`, `cpu.context_switches` | `topk`, `stack` | `sample` | the same |
| `cpu.oncpu_time` | `hist`, `rate` | `trace` | the `bpf:tp` backend |
| `cpu.topdown` | `summary`, `raw` | `count` | an issue-slot counter this cpu has not got |
| `cpu.flow` | `raw`, `timeline` | `trace` | Intel PT or CoreSight, and a decoder |

Formats are checked against the workload rather than against each other: the
counted window is rendered as `text`, `yaml`, `toml` and `ndjson`, and the
thousand million instructions the loop retired has to be a number in each
document. A profile is rendered as `folded`, `speedscope` and `pprof` — text,
JSON and a gzipped protobuf — and refused by `chrome`, which carries timelines
and not stacks.

### Do two roads to the same number agree?

`every_backend_either_measures_or_says_it_is_not_built`

**One chain has two, and they are not two roads to the same number.** That is
established rather than asserted — the scenario forces each of the twenty-one
declared backends of the fourteen sources with `--backend force:` and
classifies what comes back — and it is the test that tells whoever writes a
second collector that the comparison is now owed: the day another chain has
two, it fails and names the source.

The chain with two is `cpu.cycles`, since the fifteenth wave: `pmu` counts the
core's cycles and `pmu:sw` is the cpu clock, the degraded fallback the catalog
declares (*"CPU time, not cycles: blind to DVFS"*). They are compared by
`a_cycle_count_and_its_cpu_clock_fallback_differ_by_the_core_s_frequency`:

| Source | Collectors | What one says of the other |
|---|---|---|
| `cpu.cycles` | `pmu` over `pmu:sw` | the core's cycles per nanosecond of cpu -- 4.2 GHz on the development machine -- between half cpufreq's minimum and a fifth over its maximum; `pmu:sw`'s own difference between a 100 ms and a 500 ms burn is the 400 ms the workload's own clock burned, to 15 % |

The two do not report in the same unit, and the output does not say so yet.
Since the sixteenth wave the catalog gives `pmu:sw` `unit: ns`
(`Backend.unit`, which the loader holds to a fallback that says what it
degrades, and `Entry::backend_unit` answers), but the three places a result's
unit is chosen (`src/main.rs`, `src/triage/engine.rs`, `src/join.rs`'s
`unit_of`) still read the entry's: both results say `count`, and a reader can
add the cpu clock's nanoseconds to the hardware's cycles. The `degraded` note
on `pmu:sw` is what says it until then.
`the_cpu_clock_fallback_of_cpu_cycles_reports_nanoseconds_and_the_counter_a_count`
holds the output to the workload's own clock; it is ignored until those places
read `Entry::backend_unit` (`docs/open-findings.md`, the sixteenth wave, stream
cpu), and passes here with `--ignored` once `src/main.rs`'s does.

Seven backends are declared and not built: `cpu.profile pmu` (a cycles
profile is `cpu.cycles -m sample -e stack` since the twenty-fourth wave; this
row stays unbuilt because a `cpu.profile` sample is read as a period of
on-cpu time by `omn latency` and `omn diff`),
`cpu.oncpu_time bpf:tp`, `cpu.oncpu_time fs:cgroup`, `cpu.util fs:cgroup`,
`cpu.pressure fs:cgroup`, `cpu.flow pmu:aux:intel_pt`, `cpu.flow
pmu:aux:cs_etm`. Each has to exit 3 and name both halves of what is missing,
which is the comparison this family can make:

```console
$ omn cpu.profile --backend force:pmu -m sample@99hz -e stack -- /bin/true
warning: [cpu.profile #1] backend `pmu` is pinned, so the resolver's choice is
         bypassed; this is for the comparison bench, not for measuring
error: no collector is written for `cpu.profile` on backend `pmu` yet
$ echo $?
3
```

One thing a reader should know while choosing a backend from `omn list`:
`cpu.profile` prints `here usable through pmu`, and every run of it resolves to
`pmu:sw` with that backend's degradation attached, because `pmu` has no
collector. The two columns answer different questions and the table says so —
`HERE` is the machine, `COLLECTOR` is the build — but neither alone answers
"which backend will serve me". The resolved record does: its `chain` names
`pmu` as `unimplemented` and `pmu:sw` as `selected`.

### Does the tool say what it did not see?

`a_profile_driven_past_the_kernels_ceiling_says_what_it_did_not_take` ·
`a_poll_faster_than_the_kernels_tick_gets_the_kernels_resolution` ·
`a_target_that_exits_ends_the_window_and_names_the_reason` ·
`a_profile_of_work_done_in_another_task_finds_nothing_and_reports_no_loss`

`examples/cpu-profile-loss.yml` asks for a hundred thousand samples a second,
which no kernel will give, and the run is read for its loss block rather than
for its profile:

```
19327 events over 330.02ms, stopped because the requested duration elapsed
LOSS: 11435 dropped, 0 failed reads, 7 late ticks
   1x a round of reads took 104754353ns, longer than the 10000000ns tick
   …six more, one per late tick, each with its own duration in it…
  12x the kernel throttled profile sampling: an unknown number of samples in
      each throttled period were not taken (see kernel.perf_event_max_sample_rate)
   2x the profile ring overflowed: samples were taken and never read, so the
      stacks here are a biased subset -- the busy phases are the ones that overflow
```

Both sentences are right about what they cannot know, and the second is the one
that matters for reading the result: a ring overflows when samples arrive faster
than they are drained, which is when the target is busiest, so what is missing
is not a random third of the profile but the hottest third. The scenario checks
that the loss reaches the text a person reads *and* the JSON a harness reads,
and that the run names `kernel.perf_event_max_sample_rate`, which is the one
thing a reader can act on.

A target that exits before the window closes ends the run and says so — against
a request for five seconds, a workload that lives 200 ms produces a 200 ms
window and `stopped because task N exited`, not five seconds of nothing.

The other two are defects and are below: a poll faster than the kernel's own
tick loses nine tenths of its readings without a word, and a profile of a
program whose work is in another task finds none of it and reports no loss.

### Is the reason the right reason?

`each_refusal_names_the_thing_that_is_missing` ·
`a_mode_the_build_does_not_implement_exits_as_unimplemented` ·
`examples/cpu-out-of-reach.yml`

Three different answers, which a reader acts on differently:

| It says | It means | Exit here |
|---|---|---|
| a rejection | the request cannot mean anything, on any machine | 2 |
| no viable backend | this machine cannot serve it; another can | 2 |
| no collector | this build does not implement it; the request is fine | 3 |

Seven rejections are checked by message — an emit a source does not declare, a
mode it does not support, a target it is not attributable to, a filter field it
does not carry, a key it cannot be joined on — and each one is also checked for
what it must **not** say: a rejection that sent a reader to
`perf_event_paranoid` or to a capability would cost more than no message at
all. The two out-of-reach sources must name the requirement they lack (`slots`,
`intel_pt`) and must say the request is valid elsewhere.

The third column is the defect: the first two answers share an exit code.

---

## The four classes, and what each one found here

| Class | Scenarios | What it caught |
|---|---|---|
| **A — the capability sweep** | 3 | eighteen (source, emit) pairs the catalog offers and this build cannot reach; the exit code an unimplemented mode uses |
| **B — the backend comparison** | 2 | that this family has none to make, established rather than assumed; a per-core and a per-process file that must agree, and a session that cannot hold both |
| **C — the loss path** | 4 | a profile that misses every task but one and reports no loss; a poll that loses nine tenths of its readings silently; throttled periods counted as dropped samples; a loss reason that never aggregates |
| **D — the refusal** | 3 | two different answers sharing one exit code; a message pointing at a mechanism that has since landed; a crowded pmu reported as a target that never ran |

---

## Workloads

All of these are in `tests/scenario_cpu.rs`, and phase two can reuse them. The
test binary re-executes itself as its own workload: `omn` launches it with
`OMN_CPU_SCENARIO_WORKLOAD` set, the harness runs the one named test, and that
test does the work and exits. It is the only program on this machine whose
instruction count this repository controls.

| Spec | What it does | What it guarantees |
|---|---|---|
| `retire:N` | a `dec`/`jnz` loop of N turns | exactly 2N retired instructions, and nothing else |
| `burn:MS` | spins until `CLOCK_THREAD_CPUTIME_ID` advances MS | MS of cpu consumed, whatever the machine's load |
| `sleep:MS` | `thread::sleep` | MS blocked, no cpu |
| `split:MS` | `burn:MS` then `sleep:MS` | **half on a cpu and half blocked** — what P2 asks for |
| `sleeps:N` | N sleeps of 2 ms | at least N switches away from the task |
| `spin:MS` | spins on the wall clock | busy, for scenarios that need a cpu-bound target rather than a known amount |

Two shell workloads are used where the program has to be something other than a
test binary: a spinning copy of `/bin/sh` under a name nothing else on the
machine has (`Spinner`), used where a measurement has to find its own workload
in a system-wide or per-core view; and `sh -c '(loop) & wait'`, a program whose
work is in a child and whose parent blocks, which is what the profile defect
below is demonstrated with.

**One caveat for whoever reuses the re-executing workload.** `cargo test` runs a
test on a thread it spawned, so the work is never on the process leader. A
counting event is inherited and follows it; a sampling event does not, and
profiles that workload as almost nothing. That is defect 1, and it is why
`spin`-based shell workloads are used for the profile scenarios.

---

## What cannot be verified here, and why

- **`cpu.throttle` against a real quota.** The cpu controller is not delegated
  to this session's cgroup. The scenario and the example exist; the skip names
  the missing delegation, and a machine with it runs them.
- **`cpu.topdown`, at all.** This cpu publishes no issue-slot counter, so all
  four of its categories and both of its emits are out of reach. The refusal is
  checked instead, including that it names `slots` and does not blame
  privileges.
- **`cpu.flow`.** No Intel PT, no CoreSight, and no collector for either. Both
  halves would have to change.
- **`sample` mode on the seven counting sources.** Declared by the catalog,
  refused by the build. What is checked is the refusal.
- **That a profile's stacks are the right stacks.** The scenarios check how many
  samples a profile took and against what cpu time, not what is in the frames.
  `tests/stack_capture.rs` owns that question, and on this machine most targets
  are built without frame pointers — the run says so as a degradation, and
  `--stack dwarf` is a different measurement with a different cost.
- **The absolute correctness of PSI.** The scenario checks that the stall share
  rises with oversubscription and is near zero without it. That the number is
  the *right* stall share is the kernel's claim, and nothing here can
  independently produce it.
- **Anything counted while the pmu is shared.** A time-shared counter group
  returns an extrapolation, and the scenarios that check a count against ground
  truth stop when the run says that happened. Nothing is lost — the tool's
  disclosure is what they read — but on a machine running eight suites at once
  those scenarios do sometimes decline to measure.
- **`cpu.migrations` against a chosen number of migrations.** Forcing a known
  number of cross-core moves means controlling the scheduler, which a scenario
  that has to share this machine cannot do. It is covered by the sweep — it
  counts, it emits, it refuses a `--cpu` target by name — and not by ground
  truth.

---

## Defects found

Ten, **five of them now fixed**: 1, 2, 3, 7 and 9, in the fifth wave, by the
stream that owns stacks and the two families that produce them. Each is marked
below with what changed and the scenario that now asserts it, and the
reproduction is left in place because that is what the assertion is written
against. `docs/fixes/stacks.md` is that stream's log.

None of them was worked around: every scenario above runs against the
tool as it is. Two of them held a defect in place — the profile that finds
nothing, and the exit code of an unimplemented mode — and each says in its own
doc comment that a failure means the defect was fixed and the scenario has to
be rewritten to the invariant. The rest check what is true today and survive
the fix.

### 1. A profile follows one task, finds nothing, and reports no loss

**Fixed** (wave five). Not by finding the work — a per-task sampling event
cannot be inherited, which is a property of the mechanism and not of this build
— but by saying so. `cpu.profile` now attaches `SampledEvent::sampled_threads`
as a degradation whenever the target is more than the one task the event
follows, with the number of tasks the run really took samples from beside it:

```
degraded: a sampling event is not inherited, so this profile followed the target
task only: threads it starts afterwards are not sampled; it has samples from 1
task, and cpu burned in a thread or a child process the target started is in no
number here
```

A degradation and not a loss, on purpose: the samples were never taken, so
there is no count of them, and `losses.dropped` staying zero is the honest
answer. `sched.switches`, which opens the same shape of event over the same
kind of target, now says the same thing. `mem.faults` is the third and belongs
to another stream; the note is in `docs/open-findings.md`.

The worst of the ten, and the one the loss class exists for. `cpu.profile`
opens a sampling event, which the kernel will not inherit — it refuses to map a
ring for an inherited per-task event — so the profile sees the task it was
opened on and nothing else. Nothing says so.

One shell, one window, two sources. The counting event is inherited and follows
the child; the sampling event does not:

```console
$ omn -f - <<'YAML'
name: the work is in the child
format: text
requests:
  - label: what the program did
    source: cpu.instructions
    target: {launch: [/bin/sh, -c, "(i=0; while [ $i -lt 200000 ]; do i=$((i+1)); done) & wait"]}
    mode: count
    emit: [summary]
  - label: where it did it
    source: cpu.profile
    target: {launch: [/bin/sh, -c, "(i=0; while [ $i -lt 200000 ]; do i=$((i+1)); done) & wait"]}
    mode: sample@997hz
    emit: [topk]
    stack: fp
YAML
what the program did  via pmu  (cpu.instructions)
  1 events over 553.34ms, stopped because the launched program exited with exit status 0
  no loss: every read succeeded and no tick ran late
  summary
    task:3730319 n=1  last 4926694019 count

where it did it  via pmu:sw  (cpu.profile)
  1 events over 553.34ms, stopped because the launched program exited with exit status 0
  no loss: every read succeeded and no tick ran late
  topk: the 1 heaviest of 1 series
     1. task:3730319      1 count  over 1 events
    nothing was left out: that is every series
```

Four thousand nine hundred million instructions in one window, and a profile of
it that took **one sample** and says *nothing was left out*. A thread pool
profiles as the thread that started it, and the flame graph looks fine.

The disclosure already exists and is never called. `src/perf.rs` has:

```rust
/// What this event does and does not see, in one line, for the record that
/// travels with the numbers.
pub fn sampled_threads(&self) -> &'static str {
    "the target task only: threads it starts afterwards are not sampled"
}
```

`grep -rn sampled_threads src/` finds the definition and no caller. The same
applies to `mem.faults` and `sched.switches`, which open sampling events the
same way. `STATE.md` says `mem.faults` "states that it sees the target task
only"; neither its catalog entry nor a run of it says so on this build.

Two shapes of fix, and the second is the one that would make the source
trustworthy: attach the degradation whenever a sampling event is opened on a
target that can have more than one task, which costs nothing and is honest; or
open one event per cpu and filter, which is what `perf record -a` does and what
`cpu.profile --system` already refuses for want of privileges.

Was pinned by `a_profile_of_work_done_in_another_task_finds_nothing_and_reports_no_loss`;
now by `a_profile_that_cannot_see_the_work_says_it_did_not_see_it`, which keeps
the workload and the counting event beside it as the independent reference and
asserts the sentence rather than the silence.

### 2. A throttled period is counted as a dropped sample, and `losses.throttled` stays zero

**Fixed** (wave five). `cpu.profile` and `sched.switches` now write
`self.losses.throttled += throttles`, which is what
`src/collect/mem/page_faults.rs` already did and what the comment above the
`sched.switches` line already claimed. A throttled run's JSON now carries
`"throttled": 2`, and `dropped` counts only what a ring reported losing.

`Losses::throttled` exists and documents exactly why it must not be folded into
`dropped`:

> Deliberately not folded into `dropped`. A drop count is a number of events the
> ring reported losing; a throttle is the kernel declining to deliver anything
> for a while, and how many events happened inside that window is not knowable
> from userspace. Counting one throttle as one drop claims a precision that does
> not exist […]

`src/collect/cpu/profile.rs` does exactly that:

```rust
if throttles > 0 {
    self.losses.dropped += throttles;
    self.losses.note("the kernel throttled profile sampling: …");
}
```

so the JSON a harness reads says the run lost 98 samples and was never
throttled, while the reasons underneath say it was throttled twenty times:

```json
{ "dropped": 98, "failed_reads": 0, "late_ticks": 3, "undecoded": 0, "throttled": 0 }
   20x "the kernel throttled profile sampling: an unknown number of samples in
        each throttled period were not taken (see kernel.perf_event_max_sample_rate)"
```

`Losses::throttle()` is the method that does the right thing, and the fix
already exists in this repository: `src/collect/mem/page_faults.rs` writes
`self.losses.throttled += throttles` with a comment saying precisely why. Two
collectors do not, and one of them says it does —
`src/collect/sched/switches.rs` explains in the comment above the line that a
throttle "is counted in throttled periods and labelled as such", and then adds
it to `dropped`. A consumer filtering on `losses.throttled != 0`, which is what
the field is for, sees a clean run; the twenty periods are reported as a
precision (`98` samples) that nobody has.

Pinned by `a_profile_driven_past_the_kernels_ceiling_says_what_it_did_not_take`,
which counted a throttle whichever field it landed in and so survived the fix.
It now asserts the field as well — `losses.throttled` is what a consumer
filters on — and that any `dropped` count is explained by a ring overflow, so
a number of periods can never again be reported as a number of samples.

### 3. `cpu.util` loses nine tenths of its readings to the kernel's tick and says nothing

**Fixed** (wave five). `cpu.util` counts its reads and the ones that found the
file unmoved, and states the result — measured, rather than predicted from the
interval:

```
degraded: 426 of 475 reads found `/proc/stat` unmoved and produced no share,
because the kernel credits cpu time in ticks of 10 ms and a window where nothing
moved is not a window that was idle; this polls every 1 ms, so the series
carries the kernel's resolution and not the one that was asked for
```

`cpu.oncpu_time` keeps its own sentence, which is derived from the interval and
made before the run rather than after it. The two are complementary, and what
this defect was about is that only one of the two readers of this tick said
anything at all.

`/proc/stat` credits cpu time in ticks of 10 ms. `cpu.util` reads it, and when
two reads fall inside one tick nothing has moved, so the collector emits no
event — which is right, because a share of a window where nothing moved is not
zero. What is missing is the disclosure. Ask for a reading every millisecond of
a core with one spinning tenant:

```console
$ omn cpu.util --cpu 23 -m poll@1ms -e summary -d 500ms
cpu.util  via fs:proc  (cpu.util)
  51 events over 500.97ms, stopped because the requested duration elapsed
  no loss: every read succeeded and no tick ran late
  summary
    cpu:23 n=51    min 1.000  mean 1.000  max 1.000  last 1.000
```

Five hundred readings were asked for and fifty-one arrived. The answer is right
— the core is busy, and the readings that exist are accurate — but the series a
reader plots has a tenth of the resolution they requested and nothing in the run
says why.

`cpu.oncpu_time`, reading the same tick from `/proc/<pid>/stat`, does say it:

```
degraded: the kernel credits cpu time in ticks of 10 ms and this polls every
1 ms, so each reading is one tick -- up to 1000 % -- away from what was
consumed; the `bpf:tp` backend is the one that resolves below a tick
```

Two readers of the same kernel tick with two conventions, which is the shape
`STATE.md` already records for the closing read of a polled counter. The
cheapest fix is `cpu.util` borrowing that sentence; the honest one is the poll
layer refusing, or reporting, an interval below what the source can resolve.

Pinned by `a_poll_faster_than_the_kernels_tick_gets_the_kernels_resolution`,
which asserts the accuracy, and now asserts the disclosure too: that it exists,
that it names the tick, and that it is about the bulk of the reads rather than
a stray one. On a machine so loaded that no two reads fall inside one tick the
scenario skips, with the late-tick count that shows why.

### 4. A mode the build does not implement exits like a failure

> **Fixed** (stream A). A refusal raised by a factory is a pure function of the
> resolved request -- nothing has been launched, opened or read by the time it
> answers -- so it cannot be about this machine, and it exits 3. The message is
> unchanged and the reproduction below still produces it.
> `a_mode_the_build_does_not_implement_exits_as_unimplemented` asserts the code.

```console
$ omn cpu.cycles -m sample@99hz -e hist -- /bin/true
error: cannot start collecting `cpu.cycles`: `cpu.cycles` in `sample@99hz` mode
       needs the sampling path, which lands with stacks; `--mode count` reads
       the same counter exactly
$ echo $?
1
```

The request is valid and the machine has the counter; what is missing is a
collector, which is the statement `NoCollector` makes and exits 3 for. `STATE.md`
has this as an open item. It affects all seven counting sources of this family,
and a mode the catalog declares with no collector behind it is why fourteen of
the eighteen unreachable (source, emit) pairs above are unreachable — twelve of
them waiting on `sample`, two on `trace`.

The message has also gone stale: the sampling path *has* landed — `cpu.profile`
uses it, and stacks with it — so a reader is sent to a mechanism that exists.
What is missing now is a sampling collector for counting sources, which is a
different sentence.

Pinned by `a_mode_the_build_does_not_implement_exits_as_unimplemented`.

### 5. `--duration` beside `-f` is accepted and discarded

> **Fixed** (stream A). The command line may replace what a session holds for the
> whole of itself -- `duration`, `budget`, `format`, `out` -- and every other axis
> names one request, which a file may have many of, so the parser refuses those
> beside `-f` by name. A replaced duration is reported as a note and reaches
> `--dump-request`. Asserted by
> `a_duration_beside_a_session_file_replaces_the_sessions_own`.
> The consequence recorded below no longer holds: a scenario *can* shorten an
> example's window from the command line.

```console
$ time omn -f examples/cpu-known-work.yml -d 100ms
… 1.22s
$ omn -f examples/cpu-known-work.yml -d 100ms --dump-request | grep -c duration
0
```

The session names no duration, the command line names 100 ms, the run lasts as
long as the workload does, and the dumped request — the thing a reader checks
when they want to know what they asked for — carries no duration at all. Where
the session *does* name one, it wins and the flag is silently ignored:
`examples/cpu-saturation.yml -d 200ms` runs for its own two seconds.

Either answer is defensible; being told is not optional. This repository's own
rule is that an invalid combination is refused with the reason and never
degraded silently, and a flag that leaves no trace is the silent case. The same
question applies to every other axis a command line can name beside `-f`.

The practical consequence for this wave: a scenario cannot shorten an example's
window from the command line, so the sessions of this family are written short
enough to be run as they stand — which is why `cpu-saturation.yml` is two
seconds and `cpu-per-core.yml` is one.

### 6. A rejection and a machine that cannot serve the request share an exit code

> **Fixed** (stream A). `has no viable backend` exits 1 and the hint is per case;
> `exit_for` in `src/main.rs` is the one place the four answers map to codes.
> `each_refusal_names_the_thing_that_is_missing` now asserts the code beside the
> message it was already pinning.

`BRIEF.md` gives three answers with three exit codes. The build has three
answers and two codes:

```console
$ omn cpu.ipc -m count -e hist -- /bin/true
error: [cpu.ipc #1] `cpu.ipc` cannot emit `hist`
$ echo $?                                            # a rejection
2
$ omn cpu.topdown -m count -e raw -- /bin/true
error: `cpu.topdown` has no viable backend on this machine: …
$ echo $?                                            # this machine, not this request
2
```

`src/main.rs` returns `EXIT_REJECTED` for a resolver error, next to a hint that
says the opposite of what the code means:

```rust
Err(error) => {
    eprintln!("error: {error}");
    eprintln!("  hint: the request is valid; it is this machine that cannot serve it. …");
    return Ok(ExitCode::from(EXIT_REJECTED));
}
```

The messages are good and a person can tell them apart. A caller cannot: a CI
harness that runs a suite of sessions across a fleet has no way to separate "this
session is broken, fix it" from "this machine is the wrong machine for it", and
those need opposite responses. The hint is one line above the wrong code, which
suggests the code rather than the intent is what slipped.

Pinned by `each_refusal_names_the_thing_that_is_missing`, which asserts the
messages and records the codes.

### 7. A profile costs a second of wall clock before it samples anything

**Fixed** (wave five), and the candidate below was half of it. Measured again,
same machine, same debug build:

```
cpu.profile -d 50ms  --stack none   0.085 s     (was 1.05 s)
cpu.profile -d 400ms --stack fp     0.425 s     (was 1.41 s)
cpu.instructions -d 400ms           0.430 s     (unchanged)
```

Two causes, and the second is why `--stack none` paid the same second:

1. `KernelSymbols::read` ran inside every `Symbolizer::for_process`. It is now
   a process-wide `OnceLock`, read at most once per run and only when a kernel
   frame is actually looked up. The table describes the machine rather than a
   process, so a second copy of it was a second copy of the same answer.
2. `Unwinder::notes` asked `kernel_symbols_available()` at the end of every
   run, and that question *is* the parse. A run with no kernel frames cannot
   lose their names, so it is now asked only by a run that walked into the
   kernel — which is what leaves the file unread by a profile that asked for no
   stacks at all.

This closes most of finding 19 of `docs/open-findings.md` as well; defect 3 of
`docs/scenarios/sched.md` has the machine-wide measurement.

Pinned by `a_profile_costs_its_window_and_not_a_fixed_second`, whose reference
is a counting run of the same length over the same target — same startup, same
probe, same teardown, differing only in the mechanism — so nothing in it
depends on how fast this machine is.

```
cpu.profile -d 50ms  --stack none   1.05 s
cpu.profile -d 400ms --stack fp     1.41 s
cpu.instructions -d 400ms           0.42 s
```

About one second, flat, independent of the window, the frequency, the target and
whether stacks were asked for at all. A counting run of the same length costs
twenty milliseconds over its window.

The candidate is `Symbolizer::for_process`, which every sampling collector
builds at attach and which calls `KernelSymbols::read()` — a parse of
`/proc/kallsyms`, **395 318 lines on this machine**, in a debug build, whether or
not the request wants frames. `--stack none` paying the same second is the
evidence for it; confirming it is a profiler's job and belongs to whoever owns
`src/stack.rs`.

It is worth chasing beyond the annoyance, because it is very likely a large part
of finding 19 in `docs/open-findings.md`: a machine-wide run builds one
symbolizer per process, so a per-symbolizer second is re-paid per process, and
that finding measures 0.4 s per process. Caching the kernel table across
symbolizers — it describes the machine, not the process — would be one line and
would not need the maps work that finding proposes.

For this wave the consequence is that every profile scenario costs a second it
does not spend measuring, which is most of the eight seconds
`tests/scenario_cpu.rs` takes.

### 8. A loss reason that carries a measurement is a new reason every time

> **Fixed** (stream A), and with it gpu 5, io 5, net 2 and proc 6: five families
> found one line of `drive()`. The reason is the kind and the magnitudes live in
> `Losses::overrun` -- the tick, the worst overrun, the total -- joined back into
> one line when the run ends. Asserted by
> `a_run_that_was_late_many_times_explains_it_once`.

`Losses::reasons` is a map from the reason to how often it happened, and the
late-tick reason embeds the duration that made it late:

```
LOSS: 11435 dropped, 0 failed reads, 7 late ticks
   1x a round of reads took 104754353ns, longer than the 10000000ns tick
   1x a round of reads took 10610408ns, longer than the 10000000ns tick
   1x a round of reads took 11061745ns, longer than the 10000000ns tick
   …
```

Seven late ticks, seven lines, each counted once. The counted reasons beside
them — the throttles and the overflows — aggregate exactly as the map is meant
to: `12x`, `2x`. A run long enough to be late a thousand times prints a thousand
lines and carries a thousand keys in its JSON, and the one number a reader wants
from them, "how late were they", is not there because each is its own key.

A reason and its measurement are two things: the reason belongs in the map, the
duration belongs on the count — a longest, a total, or a histogram. This is
three hundred milliseconds of a busy profile; nothing here has run for an hour.

### 9. A crowded pmu is reported as a target that never ran

**Fixed** (wave five). The two causes of `running_ns == 0` are told apart by
`enabled_ns`, which for a per-task event advances only while the task is on a
cpu: zero is a target that never got one, and non-zero is a target that ran
while its counters did not. The message now says which, and the second carries
the window the target did run for:

```
`cpu.ipc` was never scheduled onto a cpu during the window, although the target
ran for 140.93 ms of it: its counters could not fit on the pmu beside what else
is open, and a group is crowded out whole. There is no count, which is not the
same as a count of zero

`cpu.instructions` was enabled for no time at all: the target did not run during
the window, so there is no count, which is not the same as a count of zero
```

Pinned three ways, because only one of the two states can be produced on
demand. `a_counter_over_a_target_that_never_ran_says_that_is_what_happened`
drives the idle one — a process asleep for the whole window — and asserts that
the message names it and does not blame the hardware. Two unit tests in
`src/collect/cpu/counters.rs` drive both branches against readings the kernel
would produce. And the scenario below asserts the crowded message wherever this
machine really does evict the ratio's group, which it does under the fan-out
and not on request: ten `cpu.ipc` groups over one workload are time-shared at
20 % duty here rather than evicted.

A single counter that shares the hardware is time-shared and scaled, and the
run says so. A *group* of two, which is what `cpu.ipc` opens so that its ratio
is a ratio of one window, is crowded out entirely — and what the run says then
is about the workload rather than about the pmu. Both requests below are the
same session over one launched program:

```console
$ omn -f examples/cpu-known-work.yml        # with six other counter groups open
what it retired  via pmu  (cpu.instructions)
  1 events over 140.93ms, stopped because the launched program exited with exit status 0
  degraded: `instructions` on task 3880813 shared the hardware with other counters
            and was on for 50 % of the window: its value is scaled from what was
            counted, not counted
  summary
    task:3880813 n=1  last 1011426627 count

what it got per cycle  via pmu  (cpu.ipc)
  0 events over 140.93ms, stopped because the launched program exited with exit status 0
  LOSS: 0 dropped, 1 failed reads, 0 late ticks
    1x `cpu.ipc` was never scheduled onto a cpu during the window -- the target
       did not run -- so there is no count, which is not the same as a count of zero
```

The target ran: the request above it counted a thousand million instructions in
the same window, of the workload that retires exactly that. The sentence "the
target did not run" is one of two causes of `running_ns == 0` and the code picks
it, and from those two numbers alone they cannot be told apart — so the message
should name both, the way the multiplexing note four lines above it does. A
reader who gets this while their program is plainly working will look at their
program.

The scenario reads it correctly: `a_known_number_of_retired_instructions_…`
stops with "the ratio's counter group was never scheduled onto the pmu" rather
than believing either story — and now asserts, before it stops, that the reason
names the pmu. The request above it counted the workload in the same window, so
the target demonstrably ran, which is what makes that assertion ground truth
rather than a check on wording.

### 10. A JSON run shares stdout with the program it launched

> **Fixed** (stream A), as a warning rather than a refusal: the run is legitimate
> and redirecting the target's output is the other way out. Text is left alone.
> Asserted by `a_structured_run_that_launches_a_program_says_they_share_the_stdout`.

```console
$ omn cpu.instructions -m count -e summary --format json -- /bin/sh -c 'echo NOISE'
NOISE
{
  "results": [ …
```

Anything the target prints lands in the middle of the document, so
`omn … --format json -- ./prog | jq` works exactly until `./prog` says
something. There is a way out and the run names it — `-o DIR` writes
`results.json`, `resolved.json` and `cost.json` — but nothing warns, and the
failure appears as a parse error in whatever consumed the pipe. Redirecting the
target's output is what every scenario in `tests/scenario_cpu.rs` that launches
a program has to do.

The mildest of the ten, and listed because a harness is the reader this format
has.

### 11. A poll's first share covers a few milliseconds

> **Fixed** (the twenty-fourth wave, stream cpu): a poll's first read is its
> baseline and produces no event, as `mem.pressure` and `io.pressure` already
> did. Asserted by `the_first_read_of_a_poll_is_a_baseline_and_emits_nothing`
> in `util.rs` and `pressure.rs`, and through the binary by the reference
> suite's `defect_hold`, which now reports the defect gone.

`cpu.util` and `cpu.pressure` took their baseline in `start`, and the runner
samples immediately after it: the first event was a share of a few
microseconds to a few milliseconds (seen 0.0 and 0.5), and it entered the
summary's `min` and `mean` beside the full windows -- `-d 4s` on a 22 % busy
machine read a mean of 0.155 against 0.19 over the four real windows
(`docs/fixes/w22-sysstat.md`, defect 2). Forty runs each at `poll@50ms` for
175 ms now give four events every time, the first 50.2 to 50.5 ms after the
window opens; before, the hold test found the leading event within forty.

`cpu.pressure` was rewritten onto the shared PSI reader in the same change,
and names its attributes as its siblings do -- `stall_ratio`, `stalled_ns`,
`interval_ns`, `full_stall_ratio`, `some_avg10` -- where it said `some`,
`avg10`, `total_us` (gap 3 of the same handoff).

---

## What `cpu` contributes to a crossing

For phase two, in one line each.

- **To P2 (compute, end to end):** `cpu` knows how much cpu a task got and
  where it went while it had it; it knows nothing about why it stopped. The key
  is `task`, carried by every scenario above. `cpu.profile` beside
  `sched.offcpu` on that key is the whole of P2's first half, and the workload
  for it is `split` — half on a cpu, half blocked, with both halves a number
  this repository chose.
- **To P4 (crossed families):** `cpu.instructions` is the most stable number in
  the project — within 0.2 % of a thousand million here, and `docs/families/
  cpu.md` measures a run-to-run spread of 0.012 % — and is therefore the right
  denominator for a confounder. `power.freq` and `power.throttle` against it
  answer whether two runs are comparable at all — `cpu.cycles` carries the
  caveat that it is frequency-dependent, and `cpu.instructions` does not.
- **The limit to plan around:** a session observes one target, so a per-core
  series and a per-process series cannot be collected in one run, and
  `cpu.util --cpu N` cannot be joined to `cpu.oncpu_time --pid` inside the tool
  today. Both scenarios above that need the pair run `omn` twice.

---

## What these scenarios cost

`tests/scenario_cpu.rs` is nineteen scenarios and about **nine seconds**, which
is more than the rest of the suite and worth knowing where it goes. Each
measured on its own:

| Scenario | Cost | Why |
|---|---|---|
| `a_profile_renders_…` | 2.8 s | four renderings, three of them run together, each paying the profile's fixed second (defect 7) |
| `a_profile_driven_past_…` | 2.7 s | two runs of a throttled profile, same fixed second |
| `a_profile_of_work_…` | 2.5 s | a session with a profile in it |
| `every_example_…` | 2.1 s | nine sessions, run as they stand; `-d` beside `-f` is ignored (defect 5), so `cpu-saturation.yml` is two seconds of PSI |
| `the_samples_of_a_profile_…` | 1.6 s | a profile again |
| `every_mode_and_emit_…` | 1.5 s | 93 combinations, eight at a time |
| `every_backend_…` | 1.2 s | 21 backends, eight at a time |
| `pressure_rises_…` | 1.2 s | forty-eight spinners and two windows |
| the other eleven | under 1 s each | |

Five of the eight slowest are paying defect 7: about a second each, before
anything is sampled.

The scenarios that measure a share or a duration take the machine to themselves
through a lock, because this one runs eight streams' suites at once and a
spinning workload on a loaded machine really does get less cpu than its wall
clock — the lock is the other way
round from the usual reading, and the comment on it says why.

Two of them also want a core to themselves, and they take the **second to
last** rather than the last: `tests/family_sched.rs` pins its spinners to the
last one, and so does every other worktree of this wave, eight of which are
running that same suite on this machine. Two suites pinning to one core is one
suite measuring the other's load. During this work `make check` failed twice in
files this stream does not own —
`family_sched::tasks_sharing_one_cpu_wait_the_share_the_scheduler_owes_them`
saw two tasks on the core it had pinned one to, and both failures passed on
their own immediately afterwards. Worth knowing if the fan-out's green runs
look intermittent: the collision is between worktrees, not inside one.

One more observation for the coordinator that is not this family's:
`tests/encoders.rs::a_known_allocation_lands_on_the_chrome_trace_at_the_time_it_was_made`
failed once during a `make check` on this worktree and passes on its own and on
every run since. Its message was `the 64 MiB allocation is not the step in the
trace: 9818112 before, 9818112 after`, which looks like a measurement disturbed
by a loaded machine rather than anything these scenarios changed.
