# Stream sched, eighteenth wave: the sched collectors follow the target

`docs/dev/STATE.md` item 5 ("After the seventeenth wave") and `scenario_sched`'s fixed
probe name (item 8). Branch `feat/w18-sched`, four commits on `9cd5409`:

- `67c4067` test(sched): the lock workload's name is the run's own
- `0ecdb09` feat(sched): the sched collectors follow the target, and a wait
  is the kernel's
- `68a8703` fix(stack): a symbol is named without the version its .symtab
  appends. Once `sched.offcpu` asked debuginfod, the `sqlite3-stripped`
  case named `clock_nanosleep` one way and `clock_nanosleep@@GLIBC_2.17`
  the other. A unit test covers it.
- this handoff

## 1. What landed

**The target's descendants are followed** (`src/bpf/sched_common.rs`,
`Descendants`, `push_followed_filter`). A program on `sched_process_fork`
adds a child's thread group to a hash map when its parent's is the root's
or already in the map. The fork tracepoint fires inside `kernel_clone`,
before `wake_up_new_task`, so a child is followed from its first wait. A
program on `sched_process_free` removes a thread group when its leader's
`task_struct` is freed. The filters compare the root as a constant and look
everything else up in the map. Processes the target already had when the
run attached are read from `/proc/<tgid>/task/*/children` after the fork
program is attached. That covers `sched.runqlat`, `sched.offcpu`,
`sched.wakeup` (as wakee and as waker), `sched.switches` (`bpf:raw_tp`) and
`sched.migrations`. A process the map had no room for is a loss note in
each collector. For a process target, renames are read machine-wide,
because children exec. `sched.runqlat` also attaches its wakeup program to
`sched_wakeup_new`, so a task's first wait (from its fork) is counted, as
the kernel's `run_delay` counts it.

**A wait is the kernel's own account** (`KernelClock`, `push_rq_clock`,
`push_last_queued`, `push_run_delay`, `push_kernel_wait`). At the switch
that ends a wait, the program reads `rq->clock` (through
`task->se.cfs_rq->rq`) and `sched_info.last_queued`, the two values
`sched_info_arrive` subtracts a few instructions later under the same lock.
It adds what `sched_info.run_delay` grew by since the wait began: a task the
load balancer moves while it is queued has its wait closed once per cpu.
`sched.runqlat`'s rows (bucket and `total_ns`) and `sched.offcpu`'s
`queued_ns` are now that account. The tracepoints' figure (`sched_wakeup`,
or the preempting switch, to the switch that runs the task) stays on every
row as `wake_to_run_ns`. A wait with no stamp is no wait, as the kernel
counts it, and is noted (`unstamped`). A kernel without the fields keeps the
tracepoints' clock and says so in a note.

**A block starts where `__schedule` was entered.** It used to start at the
switch, after the next task was picked. A program on `sched_entry_tp`
(6.16) stamps a per-cpu slot, and the switch program reads it, clears it and
rejects it when it is older than 1 ms. Without the tracepoint the block
starts at the switch, and a note says so.

**`sched.offcpu`'s stacks are named through debuginfod.** `--symbols
debuginfod` now reaches `sched.offcpu` (`stack::NAMED_BY_CHOICE`;
`reads_symbols` and the new `stack::symbol_readers()`). Its symbolizers are
built with the debuginfod client, and a request with no `DEBUGINFOD_URLS` is
refused at attach, before the target runs. Every followed process is named
against its own maps: they are read on every tick, and again on a drain for
each process a new stack belongs to, so a child that execs and exits between
ticks is still named. `stack::names_fact` writes the facts line for both
`cpu.profile` and `sched.offcpu`. A module named by its **dynamic symbols
only** now triggers it.

**`scenario_sched`'s lock workload has a name per run**: `omnlk` plus eight
hex digits of a hash of the checkout and the test's pid. The run uses a
scratch copy of `examples/sched-lock-shared-file.yml` that filters on that
name, and the test asserts that the example filters on `omnlockprobe`
exactly once.

**Catalog** (`catalog/sched.yaml`, non-cost lines): the five `bpf:raw_tp`
backends require `sched_process_fork(task_struct *, task_struct *)` and
`sched_process_free(task_struct *)`, and runqlat also requires
`sched_wakeup_new(task_struct *)`. New caveats on runqlat, offcpu and
wakeup say what is followed and whose clock a wait is read on.

`scenario_sched`'s round-trip and half-blocked scenarios now read the
driver's blocks. `sched.offcpu` sees the responder as well now, and the
responder blocks once per round trip too.

## 2. Shown, and reasoned

**Shown.**

- The 1.7 us, reproduced and placed. The workload was a C program doing
  20 000 `nanosleep`s of 50 us, compared with its own
  `/proc/thread-self/schedstat` around the loop. On the quiet machine, the
  tracepoints counted 1 630 ns runnable per wait and `schedstat` counted
  121 ns. The tracepoint-based blocked row was 2 167 ns per wait short of
  the kernel's residual (wall − cpu − run delay). After the change, under
  load average 20 to 50:

  | | kernel run delay | omn queued | kernel blocked | omn blocked |
  |---|---|---|---|---|
  | run 1 | 35 943 | 35 896 | 105 383 | 105 683 (+0.3 %) |
  | run 2 | 18 900 | 18 672 | 102 555 | 103 789 (+1.2 %) |

  (ns per wait. The kernel's run delay also holds the few preemption waits.)
  `sched.runqlat` over the whole run is 459.8 ms against the kernel's
  459.2 ms, and 325.0 against 323.3.
- Three ways to place the boundary, measured in one run each, on the blocked
  row against the kernel's residual: start at `__schedule` entry with the
  kernel's queue wait gave +1.5 to +1.9 us per wait; start at the switch
  with the kernel's queue wait gave −1.7 to −2.2 us; the scheduler's clock
  at both ends gave −2.5 to −4.5 us. On the quiet machine the first gives
  about 0. That is why the first one landed.
- Without the `run_delay` growth, `sched.runqlat` was 17 % short of the
  kernel under load (50.5 ms against 60.6 ms). With it, 0.1 to 0.5 %.
- Children followed, with exact waits:
  `a_launched_programs_children_are_followed_and_wait_what_their_schedstat_says`
  (three forked children, 3 000 sleeps of 20 us each). Typical output:
  *"child 453178: 3013 waits, 8728965 ns by its schedstat; counted 3010
  waits, 8728965 ns (17449444 ns by the tracepoints)"*. The total matches
  to the nanosecond in nine of nine children over three runs, and the
  tracepoints' figure is 2 to 40 times larger. Before the change, a child
  had no row at all.
- debuginfod, local:
  `a_stripped_blocks_stack_is_named_from_debuginfod_only_when_asked` builds
  a stripped program with frame pointers and serves the unstripped build by
  build id over loopback. Without the flag, nothing is asked, the static
  function is not named, and the note says *"its dynamic symbols only"*.
  With it, `omn_calls_the_block` is named, the note names the server and
  the build id, and the server is asked once. With no `DEBUGINFOD_URLS` the
  request is refused before the program runs.
- debuginfod, real server (`DEBUGINFOD_URLS=https://debuginfod.archlinux.org`,
  by hand): `omn trace sched.offcpu -e stack -- /usr/bin/sleep 0.3` gives
  `sleep+0x2835;__libc_start_main;libc.so.6+0x27781;...;clock_nanosleep`,
  with *"names came from -- sleep: its dynamic symbols only ..."*. With
  `--symbols debuginfod` it gives
  `_start;__libc_start_main@GLIBC_2.2.5;__libc_start_call_main;main;xnanosleep;rpl_nanosleep;clock_nanosleep@@GLIBC_2.17`,
  with both build ids from the server, in 1.25 s cold.
- The three lock scenarios pass with the per-run name.

**Reasoned, not shown.**

- *Why* the kernel's and the tracepoints' accounts part. The likely cause is
  the `rq_clock_skip_update` after a wakeup preemption, plus idle exit.
  Rather than modelling the kernel, the change reads the kernel's own
  values. On a loaded machine the blocked row still overshoots the
  `schedstat` residual by about 1.5 us per wait. The kernel counts that
  time as the task's cpu time, which is consistent with a stale
  `exec_start`. This is the residual's resolution, stated in the caveat
  rather than corrected.
- A task preempted with a sleeping state still set (`preempt` true,
  `prev_state` not zero) is counted as a block by `sched.offcpu`, which
  reads the raw state and not the `preempt` argument. The kernel also keeps
  it out of `run_delay` (`sched_info_depart` stamps only `TASK_RUNNING`), so
  the rows stay consistent with `schedstat`. These are the *"blocks ended
  without this run having seen the wakeup"* in the notes, a handful per
  run. Left as it was.
- Pid reuse: a thread group stays in the map until its leader's
  `task_struct` is freed (`sched_process_free`, after RCU). A pid reused
  inside that grace period would be followed. If the root exits under a
  `--pid` run and its pid is reused, the new process would be followed by
  the root comparison. Neither was observed.
- Cost. The new programs are the fork and free programs (every fork and
  free on the machine, one map lookup each) and, for `sched.offcpu`, the
  `sched_entry_tp` program (every `__schedule` on the machine: one per-cpu
  array write). The switch programs gained four probe reads for each switch
  of a followed task. Not measured; see section 4.

## 3. Tests

- `tests/family_sched.rs`: two new tests (above), and a third in the fix
  round (see Review). All 17 pass.
  `tasks_sharing_one_cpu_wait_the_share_the_scheduler_owes_them` (flaky
  tally #6) failed once in the file's run at load average 50 and passed
  alone.
- `tests/scenario_sched.rs`: all 52 pass. Two tests failed once each in a
  whole-file run and passed when rerun alone:
  `one_known_number_of_round_trips_is_found_by_four_sources` (tally #1;
  passed six times alone) and
  `a_targets_blocking_stack_is_named_while_another_process_floods_the_stack_map`
  (tally line 98).
- Unit tests: `bpf::{offcpu,runqlat,sched_common,wakeup,switch_count,migrate}`,
  `collect::sched`, `stack` (155). The layout tests cover the new stack
  slots and value fields. The atomics counts are now 6 for offcpu and 3 for
  runqlat.

## 4. What I could not do, and why

- **The cost of the new programs** needs `kernel.bpf_stats_enabled`, which
  is machine-wide (section 5). The bench's bands are the bench stream's.
- **Kernels older than 6.16** (no `sched_entry_tp`) and kernels without
  `CONFIG_SCHED_INFO` or `CONFIG_FAIR_GROUP_SCHED`: the fallbacks are
  written, built and noted, but only ran here on 7.2. That needs the matrix,
  which runs alone at the end of the wave.
- **`sched.lock`, `sched.futex`** filter the current task with their own
  `current_and_filter`. Following descendants there means a map lookup on
  the current thread group, the same shape as `push_followed_filter`, and
  is not done. `sched.irq`, `sched.numa_balance` and `sched.mutex` have no
  per-process filter to extend.
- **`omn latency --symbols`** does not reach the triage's `sched.offcpu`
  request, because `src/cli.rs` and `src/triage/latency.rs` are not my
  files. Filed below.
- `main.rs`'s refusal of `--symbols debuginfod` lists `UNWOUND_BY_CHOICE`,
  so it does not name `sched.offcpu` among the sources that read the flag.
  Filed below.

## 5. Privileged runs wanted (the coordinator, alone)

```console
cd /home/moha/repos/omniscient && make
# 1. what the new programs cost inside a target (fork/free on every fork and free, sched_entry_tp on every __schedule)
sudo -n /usr/bin/sysctl kernel.bpf_stats_enabled=1
target/debug/omn trace sched.offcpu -e summary -- /bin/sh -c 'for i in $(seq 2000); do /bin/true; done'
target/debug/omn trace sched.runqlat -e summary -- /bin/sh -c 'for i in $(seq 2000); do /bin/true; done'
sudo -n /usr/bin/sysctl kernel.bpf_stats_enabled=0
# 2. the sched files at paranoid 2: with the capabilities every test runs, without them every eBPF test skips with the reason
sudo -n /usr/bin/sysctl kernel.perf_event_paranoid=2
cargo test --test family_sched --test scenario_sched 2>&1 | grep -E 'skipped|test result'
touch src/main.rs && cargo build --workspace && getcap target/debug/omn   # (empty)
cargo test --test family_sched --test scenario_sched 2>&1 | grep -E 'skipped|test result'
sudo -n /usr/bin/sysctl kernel.perf_event_paranoid=-1 && make
```

Read the `cost:` line's *"inside the target"* figure for both sources. The
first is the run that says whether `sched_entry_tp` on every `__schedule`
is worth its 0.65 us a block.

## 6. Decision lines for `docs/DECISIONS.md`

- **A sched wait is the kernel's own account** (eighteenth wave, stream
  sched): `sched.runqlat`'s waits and `sched.offcpu`'s `queued_ns` are read
  from `sched_info` at the switch (`rq->clock - last_queued`, plus
  `run_delay`'s growth over the wait), not timed between tracepoints. The
  reference every case holds a decomposition to is the task's `schedstat`,
  and the two accounts part by about 1.5 us per wait, on every wait. The
  tracepoints' figure is kept as `wake_to_run_ns`. Only these durations
  come from the scheduler's clock; timestamps stay `CLOCK_MONOTONIC`.
- **A block starts at `__schedule`'s entry** (`sched_entry_tp`), where the
  kernel stops the task's cpu account, and not at the switch after the
  pick. Kernels before 6.16 keep the switch and say so.
- **Every sched source with a per-process filter follows the target's
  descendants** in the kernel (`sched_process_fork`/`sched_process_free`
  and a map), as `cpu.profile` does in userspace. `sched.lock` and
  `sched.futex` are not done yet.
- **`--symbols debuginfod` is read by `sched.offcpu`**
  (`stack::NAMED_BY_CHOICE`): the kernel walks the stack, so where the
  names come from is the only choice there is.

## 7. Needs from another stream

### To stream triage (`src/triage/latency.rs`)

- The `runnable` row (`sched.runqlat` `total_ns`) and the `blocked` row's
  subtrahend (`sched.offcpu` `queued_ns`) are now the kernel's account, wait
  for wait. A test or a case that held them to `schedstat` with a tolerance
  for the old bias can tighten it.
- `sched.runqlat` and `sched.offcpu` now return rows for the target's
  children (each row carries `pid` and `tid`). Following a wait for a child
  into the child that ran during it (the decision before this wave) can read
  them from the same request. A verdict that summed every row of the
  request as the main thread's will now include the children's.

### To stream names (`src/cli.rs`)

- `--symbols` on `latency` and `saturate` can now be set on the
  `sched.offcpu` request: `stack::reads_symbols("sched.offcpu")` is true.

### To stream launcher (`src/main.rs`, `symbols_not_read`)

- The refusal lists `UNWOUND_BY_CHOICE`. Use `omn::stack::symbol_readers()`
  so that it names `sched.offcpu` too.

### To stream cases (`cases/**`, `tests/cases.rs`)

**`tests/cases.rs` is red on this branch until this is done.** The run
says *"`the-verdict-says-how-it-knows` is listed under open_findings and now
passes everywhere it was checked"* for `cpython-interpreter` and
`http-server-stdlib`: remove those two lines.

These `filed:` lines should now pass, and come out when they do:

- *"dynamic symbols only is not said"* (`cpython-interpreter`, for
  `cpu.profile`; `stack::names_fact`);
- *"an off-cpu stack cannot ask debuginfod"* (`sqlite3-stripped`,
  `sqlite3-durability`): part 1 has landed, part 2 is the cli line above;
- *"the sched sources follow the target's descendants"* and *"a blocked row
  called exact is short of the kernel's"* (`make-sqlite`,
  `sqlite3-durability`, `gcc-shell`).

### To stream bench (the `cost:` lines, `docs/bench.md`)

- `sched.offcpu`, `sched.runqlat`, `sched.wakeup`, `sched.switches`
  (`bpf:raw_tp`) and `sched.migrations` load two more programs for a
  process target (fork, free). `sched.offcpu` loads one more on every
  `__schedule` (`sched_entry_tp`). The switch programs read four more
  fields per switch of a followed task. Their measured bands may move.

### To stream engine (`docs/CATALOG.md`, `examples/**`)

- The sched section can say that a process target is followed into its
  children, and that a wait is `sched_info`'s account (the catalog caveats
  carry both).
- `examples/sched-round-trips.yml`'s header says `sched.offcpu` finds "N
  blocks" and `sched.runqlat` "N run queue waits" for N round trips. Both
  now follow the shell's responder subshell too, which blocks and waits once
  per round trip as well: the driver's row (`keys: [task]`) is still N, and
  the run's totals are about 2N. Suggested wording: "N blocks on the
  driver's row, and about N more on the responder's, which the run follows
  as a child of the target". The same for `sched.runqlat`. `sched.wakeup`'s
  2N is unchanged: both ends were already in it.
- `examples/sched-half-blocked.yml`'s "the driver was blocked 182.5 ms
  across 605 blocks" is the driver's row and stays true of it, but the
  run's `sched.offcpu` total now includes the responder's blocks. It can say
  "on the driver's row". `scenario_sched` reads the driver's row in both
  (`driver_blocks`, and the half-blocked tid filter).

### To the coordinator

- `docs/scenarios/sched.md`: the per-wait bias and the descendants (from the
  seventeenth wave, stream build) are fixed; the numbers are in section 2.
- `docs/dev/STATE.md` items 5 and 8 (the probe name): landed.
- The flaky tally (`docs/flaky.md`, stream green's file): #6 and #1 each
  failed once under load average 50 and passed when rerun, as section 3
  says.

## Review

The fix round answered the adversarial review of `133e0d6`:

- **Stale example comments, no request filed** (gap): filed to stream
  engine in section 7; `examples/**` is engine's.
- **Only runqlat and offcpu were held to following children** (gap): new
  test `a_targets_children_are_woken_switched_and_moved_in_its_rows`
  (`tests/family_sched.rs`). A `--pid` target forks three children a second
  after the runs attach, so the fork program is what adds them; each child
  moves itself between cpus 0 and 1 and sleeps 1 ms, for two seconds.
  `sched.wakeup`, `sched.switches` pinned to `force:bpf:raw_tp` (the
  scenarios only reach `pmu:sw`) and `sched.migrations` each must count at
  least nine tenths of each child's moves; migrations are also bounded
  above. Measured: 1 840 moves a child, counted 1 840 to 1 845 wakeups,
  3 684 to 3 687 switches, 1 840 to 1 841 migrations.
- **Two identical `MAP_DELETE_ELEM` calls** at switch-in
  (`src/bpf/offcpu.rs`): one removed.
- **The fallback note understated the bias** without `sched_entry_tp`:
  it now says about 0.7 us a block on a quiet machine and about 2 us under
  load, the figures of section 2.
- **The separation of the two clocks was not asserted**
  (`a_launched_programs_children_are_followed_...`): summed over the
  children, the kernel account's distance from `schedstat` must be smaller
  than the tracepoints'. Measured: about 25 us against 17.6 ms over three children, so the
  assertion holds with three orders of magnitude to spare; the doc comment
  says the bracket alone does not separate them under load.
- **Symbolizers were never pruned** (`src/collect/sched/offcpu.rs`): a
  process that has left the followed set (its leader freed) has its
  symbolizer dropped one tick after the tick that first saw it gone. Its
  last blocks are in that tick's drain, because the switch that writes a
  row precedes the free. Where its modules' names came from is kept for the
  facts line. Unit test
  `a_process_that_left_is_forgotten_a_tick_after_it_left`. What is left,
  and not measured: the per-tick cost still grows with the *live* set (one
  `/proc/<pid>/maps` read and one key walk per followed process), and a
  machine-wide run (`Whose::Any`) still keeps one symbolizer per process
  that ever blocked, as before this wave.
- **The lock scenarios left a copy of `/bin/sh` and a `.yml` per process**
  (`tests/scenario_sched.rs`): the session copy is removed after its run,
  and the probe is a `LockProbe` that removes itself when the scenario that
  made it ends (the three scenarios take the lock before making it, so none
  removes another's).

## 8. The last `make check`

The one `make check` at `0ecdb09` was red on
`budget::the_extra_cpu_a_faster_poll_costs_is_the_extra_cpu_the_shell_observes`
(flaky tally #14). It passed when rerun alone. Clippy and fmt are clean.

The `cargo test --workspace --no-fail-fast` that followed, at load average
30 to 50 with the other streams running: **2621 passed, 14 failed, 12
ignored**. Each failure was rerun alone once:

- passed alone: `cross_io::the_crossing_sessions_run_and_say_what_they_found`,
  `runner::a_source_attached_for_one_phase_counts_that_phase_and_nothing_of_the_other`,
  `family_sched::a_task_moved_between_two_cpus_is_counted_once_per_move`,
  `scenario_net::every_datagram_sent_to_a_closed_port_is_a_drop_with_one_reason`,
  `scenario_proc::defect_1_the_first_reading_of_a_launched_target_is_of_the_program`,
  `scenario_sched::one_known_number_of_round_trips_is_found_by_four_sources`,
  and the three `unwind` tests (`dwarf_*`, `fp_leaf_*`);
  `scenario_gpu::the_process_figure_and_the_device_figure_move_by_the_same_step`
  also passed in the same run's second listing;
- still failing alone, under load:
  `runner::a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed`
  (`cpu.profile` throttled 4 762 times, so the series is short by more than
  its ring's loss; nothing here touches it), and
  `triage::a_verdict_read_off_a_lossy_profile_says_its_amount_is_a_lower_bound`
  (the 100 kHz profile lost half its samples, so the verdict names no state
  and leaves 299 ms in no row; `runnable` is 0.005 % of the run). In the
  first run it failed naming `cpu_queue`. Both need the machine quiet to
  judge;
- `cases::every_case_names_its_known_cause_both_ways`: after `68a8703`, it
  fails only on the two `open_findings` lines this work closed (section 7,
  to stream cases).

Earlier in the session, at load average 50, `tests/triage.rs` also failed
`a_layer_this_run_cannot_serve_...`, `an_escalating_run_holds_its_budget_...`
(`sched.futex` measured at 10.3 % against a 5 % budget) and
`a_join_from_a_held_source_...`. These are budgets and escalations measured
on a saturated machine, and they did not fail in the no-fail-fast run.

After the fix round: `make check` stopped on `tests/cases.rs` alone
(`every_case_names_its_known_cause_both_ways`, the two `open_findings` lines
of section 7, to stream cases); clippy and fmt clean. The `cargo test
--workspace --no-fail-fast` that followed, at load average 3 to 13: 2193
passed in the integration and doc binaries summed, 3 failed, 10 ignored. The
three: that `cases` test, and
`runner::a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed`
and `triage::a_verdict_read_off_a_lossy_profile_says_its_amount_is_a_lower_bound`.
Those two still fail alone on a near-quiet machine, and **fail the same way
on `main` at `9cd5409`** (a scratch worktree, built with `make`, each run
once): a 100 kHz `cpu.profile` throttled by the kernel, so the series is
short by more than its ring's loss and the triage verdict names no state.
Not this stream's; the coordinator should file them with stream cpu.
