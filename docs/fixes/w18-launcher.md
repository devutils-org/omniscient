# Stream launcher, eighteenth wave: the budget and the stop

`STATE.md` "After the seventeenth wave" item 4's `src/` half and item 11's
registry line. The asks came from `docs/fixes/w17-build.md` ("a budget weighs
the launched process alone") and `docs/fixes/w17-python.md` ("a run the budget
stopped says the budget held"), filed in `docs/open-findings.md` under the
seventeenth wave's streams build and python.

Files changed: `src/collect/{mod,launch,task}.rs`, `src/main.rs`,
`tests/{budget,runner}.rs` -- and **one line outside the row**,
`src/collect/mem/mod.rs` (section 1.3, why).

---

## 1. What landed

### 1.1 The budget's denominator weighs the program's tree

`Meter::Launched` read `task::process_cpu(pid)`: the launched process's own
clock plus `cutime`/`cstime`, the children it had *reaped*. A compiler driver
in `wait4` while `cc1` runs had 4 ms in that account and `cc1`'s seconds in
none, hence *"8581.01 % of the target's cpu time"*.

- `task::descendants(pid)`: every live or unreaped descendant, parents before
  children, read from `/proc/<pid>/task/<tid>/children` of every thread (a
  child belongs to the thread that forked it); a pass over `/proc/*/stat` only
  where the kernel has no `CONFIG_PROC_CHILDREN`.
- `task::tree_cpu(pid)`: `process_cpu` of the program and of each descendant.
  Each nanosecond is in exactly one account at an instant (its own clock until
  reaped, its reaper's `cutime` after), so a walk can come out short by one
  process reaped between two reads, never long.
- `Meter::Launched(pid, HighWater)`: reads `tree_cpu`, and keeps the highest
  reading it has given (`HighWater`, equal to any other so `read_each` still
  pairs meters of one target), so that a short walk does not shrink the
  denominator.
- `Meter::read_each` gives a meter equal to an earlier one that one's
  reading **and** raises its own high-water mark to it (`Meter::agree`), so
  every `Launched` meter of one target holds the mark taken before the stop,
  not only the first.
- `Spending::read_before_the_stop`: when the run kills a program that outlived
  it, every meter is read once more just before the kill, the last moment the
  tree's processes can be read; after the kill they are reaped by whoever
  inherits them and are in no account this process can read. Counted as a
  measurement, in `measurement_ns`.
- `observed_from` now says "the launched program's cpu time and its
  descendants'", and no longer claims "clock ticks" for a program read after
  its reap (it said that before this wave too: `process_cpu` of a reaped pid
  is `None`, and the grain defaulted to `Ticks`).

The brief's wording is "the descendants the collectors watched". This weighs
**every** descendant, whichever collectors follow children: the budget is a
share of the work the target did, the reaped children were already in that
share, and a running child being out of it while a reaped one is in was the
inconsistency. Reasoned, and a decision line below.

### 1.2 A stop ends the program's tree, not its main process

`Gated::terminate` now `freeze`s the tree first -- `SIGSTOP` to the program,
then to every descendant, until a pass finds none new (a stopped process
cannot fork; a fork with a signal pending is restarted rather than completed;
bounded at 64 passes) -- then `SIGKILL`s every descendant, the program's own
process group when it leads one (`setsid`, job control), and the program, and
reaps it.

The program is **not** put in a process group of its own at launch, which is
what "ends the process group" would literally need: a group that is not the
terminal's foreground group is stopped by `SIGTTIN` the moment it reads the
terminal, so `omn -- cat` would hang. The tree walk is the "or every
descendant it started" half of the ask in `w17-build.md`. What it misses: a
descendant already orphaned before the stop (its parent exited first and it
was reparented to init or a subreaper) that also left the group. Reasoned.

Every path that terminates goes through it: the duration, a budget stop, a
failed attach, and `Drop`.

A descendant the walk found but the kill could not signal (`EPERM`: a setuid
child) is counted, `Lifetime::left_running` (in the results' JSON when not
zero), and the run says so on stderr: `warning: the stop left N process(es)
the program started running`. A count rather than the pids because
`Lifetime` is `Copy` and `src/join.rs` (stream engine's) relies on it.

### 1.3 A stopped run says so on its own line

- `collect::OVER_ITS_CEILING` ("against a ceiling of") is what both budget
  stops of `Spending::check` say, and `collect::stopped_by_budget(reason,
  source, ceiling)` recognises them -- the request's own ceiling included, so
  of two requests of one source under 1 % and 50 % only the 1 % one is
  marked as having stopped the run -- for a fixed window or a rolling span (`main.rs`'s
  `over_budget` is now that plus `ROLLING_BUDGET`).
- `MeasuredCost::stopped_by_budget` (in `cost.json` when true) and
  `MeasuredCost::stop`: one line, printed **before** the cost and verdict
  lines, once per run:

  ```
  budget: [mem.rss] --budget 0.05% was exceeded, and the run was stopped there, before its own end: `mem.rss` spent 3.70 % of the target's cpu time against a ceiling of 0.05 %, and nothing cheaper than `fs:proc:statm` is available
  ```

  whatever the verdict says about the whole window (`held`, `could not be
  checked whole`). The verdict then ends with "the run was stopped by this
  budget, as the line above says" rather than repeating the reason. The line
  carries `--budget` and `was exceeded`, which is what `tests/cases.rs`'s
  `ask_mcp` already recognises a stop by.
- A recording's stop (`record`) goes through the same `report_budgets`.

### 1.4 `WEIGHS_ITS_SAMPLES` moved into the collector registry

`collect::Factory` has `weighs_its_samples: bool` and
`Factory::weighing_its_samples()`; `collect::weighs_its_samples(source,
backend)` reads the registry, and `main.rs`'s `weighs_its_samples` calls it.
The table is gone.

The declaration itself is `.weighing_its_samples()` on the `mem.faults` /
`pmu:sw` factory in **`src/collect/mem/mod.rs`**, which no stream of this
wave owns (stream green owns `mem/{testing,swap,wss}.rs`, not `mem/mod.rs`).
Leaving it out would have regressed the mode rung the sixteenth wave landed:
the ladder would pass over `pmu:sw` in `sample` again. One line; the
coordinator may move it if a stream claims the file.

### 1.5 `tests/runner.rs`: a throttled profile's loss fails, and says why

`a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed`
failed on `main` too (checked with this stream's changes stashed:
`113395` against `108357`). Cause, read off the JSON: on a loaded machine the
kernel throttles the 50 kHz sampler (3961 to 4600 times a run here),
`cpu.profile`'s `losses_by_ring` carries each ring's drops and no throttles,
so `emit.rs`'s `placed` finds the rings' throttles (0) unequal to the run's
and marks every series with the run's loss. The first round made it skip
there; the review (section 7) showed that hid a gap in the tool behind a
`SKIP:` line the CI's skip gate does not read. It now **fails**, as on
`main`, and its assertion messages end with the throttle numbers and where
the fix is filed (stream profile, section 6). It is not in the flaky tally
because it is not flaky: it fails whenever the machine is loaded enough to
throttle, until `losses_by_ring` carries throttles.

---

## 2. Shown versus reasoned

**Shown**, each by a test that fails without the change:

- `collect::launch::tests::terminating_a_program_ends_every_process_it_started`:
  a shell with a background child shell and a waited-on one, each running a
  `sleep 30`. With `freeze` replaced by an empty list (the old behaviour) it
  fails: *"still running after the stop: [89477, 89478, 89479, 89480]"*; with
  it, all four are gone or zombies.
- `collect::task::tests::a_waiting_parent_s_tree_holds_the_cpu_its_running_child_spends`:
  `sh` waiting on a python child that burns 300 ms; the shell's own account
  stays under 100 ms, the tree's is at least 300 ms over it.
- `tests/budget.rs::a_waiting_program_is_weighed_with_the_process_it_started_and_its_stop_ends_both`:
  `omn mem.rss -d 1s -- sh -c "sh -c '<burn>'; :"`. Before
  `read_before_the_stop` the target's cpu came to **1.84 ms** (the waiting
  shell alone, the burning child read only after it was killed); now
  **994.9 ms** over the 1 s window, and the child the program started is not
  running when `omn` returns. The test asserts at least 50 ms -- a separation
  of 25x over the old figure, not a load-tuned threshold.
- `tests/budget.rs::a_run_over_its_ceiling_with_nowhere_to_go_stops_and_says_so`:
  extended -- the stopped run has the line and `stopped_by_budget: true`; the
  same run under `--budget 10000%` has neither.
- `collect::tests::a_collector_that_weighs_its_samples_serves_both_modes_and_mem_faults_does`,
  and `tests/budget.rs`'s two mode-rung tests still pass through the registry.

- `collect::tests::a_budget_stop_belongs_to_the_request_whose_ceiling_it_names`
  (fix round): the reason of a 1 % stop is the 1 % request's and not the
  50 % one's of the same source.
- `collect::tests::equal_launched_meters_keep_the_same_high_water` (fix
  round): both meters' marks equal the shared reading; without `agree` the
  second's is 0.
- `tests/runner.rs::a_profile_s_losses_land_...` (fix round): fails on this
  machine with *"the kernel throttled this profile 4327 times and its rings
  account for 0 of them, so `emit.rs` could not place the loss per ring"*.

**Reasoned, not shown**:

- The `left_running` count and its warning: provoking `EPERM` needs a setuid
  descendant under the launched program, which an unprivileged test cannot
  set up portably. The `ESRCH` case (already gone) is excluded and is what
  every existing stop test exercises.

- That the gcc driver of `make-sqlite` / `gcc-shell` now reads a sane
  percentage. The mechanism is the one the shell test shows; the cases run in
  phase B (stream cases).
- That a walk's short read is bounded by one process and the high-water mark
  absorbs it: argued in `tree_cpu`'s doc, not provoked by a test.
- The cost of the walk on a target with many threads or a large tree (each
  check reads `/proc/<pid>/task/*/children` for every member). It lands in the
  run's own `measurement_ns`, so it is surfaced, not hidden; not measured on a
  `make -j` tree.

### The suite

One `make check` (the test target runs the workspace with no fail-fast):
everything green except `cross_io::the_crossing_sessions_run_and_say_what_they_found`
(flaky tally #25: 3995 matched where 2000 were issued, two sessions side by
side on one disk; passed when rerun alone) and `cases::every_case_names_its_known_cause_both_ways`,
rerun once alone, with two failures, both from the machine's load rather than this
stream's code:
- `sqlite3-durability/commits-that-wait-for-nothing`, `both-ways-name-the-same-suspect`:
  the two ways name different kernel leaves of `on-cpu` (`bpf_trace_run2`
  against `_raw_spin_unlock_irqrestore`), both runs. A single-process target,
  so the tree meter and the tree kill change nothing for it; the leaf is the
  cost of eBPF programs running in the kernel, and ten streams are loading
  them on this machine. For stream cases, phase B.
- `sqlite3-durability/every-commit-waits-for-the-disk/mcp` (second run only;
  `one-function-dominates`' cited number failed in the first run instead):
  `cpu.profile` at 150 % against `budget: 100%` under load, and the run was
  stopped. The new stop line is what the harness quoted, which shows
  `ask_mcp` recognises it.
`cargo clippy --all-targets -- -D warnings` and `cargo fmt --check`: clean.

---

## 3. What I could not do, and why

- **`run_session`'s half of item 4** (a waiting target's budget, the
  program's stdout in the answer) is `mcp/src/**`, stream mcp's.
- **A wall-clock denominator for a target that burns nothing**
  (`docs/open-findings.md` #32): not asked of this stream, still open.
- **Orphans that left the group** before a stop are not found (section 1.2),
  and therefore not counted in `left_running` either.
  Catching them needs this process to be a child subreaper
  (`PR_SET_CHILD_SUBREAPER`), which then has to reap every orphan itself and
  makes their cpu land in its own `RUSAGE_CHILDREN`; and in the in-process
  unit tests it would change the whole test process. Not done; worth a
  decision if a case shows an orphan surviving.

---

## 4. Privileged runs wanted

None. The tests above run unprivileged; the capabilities `make` grants do not
change the launcher or the meter.

---

## 5. For DECISIONS.md

- **A launched program's budget denominator is its process tree**: the
  program's cpu, every live or unreaped descendant's, and what each reaped,
  read from `/proc/<pid>/task/*/children` and kept at its high-water mark.
  Every descendant, not only those a collector follows: the reaped ones were
  already in the denominator, and the work the target did is the tree's.
- **A stop ends the program's tree** (frozen with `SIGSTOP`, then killed),
  and the program's own process group when it made one; the program is not
  put in a group of its own at launch, because a background group stops on
  its first terminal read.
- **A run the budget stopped says so on a line of its own**, before the
  verdicts, naming the source and the percentage; `cost.json` carries
  `stopped_by_budget`.
- **A collector declares in the registry that it weighs its samples**
  (`Factory::weighing_its_samples`); no table names it elsewhere.

---

## 6. Needs from another stream

### To stream profile (`src/collect/cpu/profile.rs`): a ring's throttles in `losses_by_ring`

`losses_by_ring` builds each ring's `Losses` with `dropped` only; the sampler
keeps throttles per ring (`SampledEvent::throttled`). On a loaded machine the
kernel throttles a 50 kHz profile thousands of times a run, `emit.rs`'s
`placed` then finds the rings' throttles (0) short of the run's, and every
series is marked with the whole run's loss -- the thirteenth wave's per-task
placement undone exactly when it matters. Wanted: each ring's `throttled`
(and the retired rings' with them). Check: `tests/runner.rs::a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed`
stops skipping.

### To stream mcp (`mcp/src/runs.rs`, `mcp/src/session.rs`): the budget's stop in `run_session`'s answer

`cost.json` now has `stopped_by_budget: true` on the collection whose budget
ended the run, and stderr has the `budget: [...] was exceeded, and the run was
stopped there` line. `run_session` reports `stopped_because` from the results;
wanted: the stop said in the answer as the CLI says it, so an agent cannot
take a stopped run for a complete one.

### To stream cases (`tests/cases.rs`)

- After a budget stop, the stopped run's descendants are now killed by `omn`
  itself; the harness's own clean-up before a retry (`w17-build.md`) can stay
  as a guard for a case whose program orphans its children.
- `ask_mcp`'s stop detection (`--budget` and `was exceeded`) matches the new
  line, which comes first; nothing to change unless it wants
  `stopped_by_budget` from `cost.json` instead of stderr.

### To the coordinator

- `src/collect/mem/mod.rs`: the one line of section 1.4, outside the row.
- `STATE.md` item 11: "`WEIGHS_ITS_SAMPLES` into the collector registry" is
  done; item 4: the denominator and the stop are done, `run_session`'s half is
  stream mcp's.
- `docs/open-findings.md`, the seventeenth wave's "a run the budget stopped
  says the budget held" and build's "a budget weighs the launched process
  alone": landed here.

---

## 7. Review

The adversarial review's five findings, and what came of each:

1. **`tests/runner.rs`, the throttle skip** (defect): agreed. A skip for a
   gap in the tool, printed `SKIP:` where the skip gate reads only `skipped`
   / `SKIPPED`, turned a red test green and invisible to the CI. Reverted to
   a failure, with the cause in the assertion message (section 1.5). The
   suite is therefore red on this test on a loaded machine until stream
   profile's `losses_by_ring` carries throttles; the coordinator should file
   it in `docs/open-findings.md` if that fix does not land in this wave.
2. **`stopped_by_budget` ignores the ceiling** (nit): fixed; it takes the
   request's ceiling and matches `against a ceiling of {ceiling} %`, which is
   the request's `budget.percent()` formatted the same way the reason is.
   Two requests of one source under the *same* ceiling are still
   indistinguishable, and their stop lines are then identical, so
   `report_budgets` says it once.
3. **`read_each` and later equal meters' high water** (nit): fixed with
   `Meter::agree` (section 1.1), and a test.
4. **`src/collect/mem/mod.rs` outside the row** (nit): unchanged; still the
   coordinator's to accept (section 6).
5. **A process the stop cannot signal is silently left** (nit): now counted
   and said (section 1.2). The orphan that left the group is still not found;
   that stays a decision line and section 3's open item.

### The suite after the fix round

One `make check`: red in the library tests on two load-dependent tests
outside this stream's files (`cpu::profile::...finds_this_thread_for_the_cpu_it_burned`,
284 samples where 199 were expected; `perf::...an_ibs_sample_says_which_level_answered_the_load`,
too few samples), at a load average of 37. Then one `cargo test --workspace
--no-fail-fast`: **2193 passed, 5 failed** (the two above passed there):

- `runner::a_profile_s_losses_land_...`: the expected failure of section 1.5.
- `budget::the_extra_cpu_a_faster_poll_costs_...` (flaky tally #14),
  `scenario_proc::defect_1_the_first_reading_...` and
  `scenario_mem::a_walk_that_fits_in_l1_...` (both in the tally): each
  passed rerun once alone.
- `triage::a_verdict_read_off_a_lossy_profile_says_its_amount_is_a_lower_bound`:
  failed again alone. `limit` is null because `on-cpu` is 127.51 ms of a
  300 ms burn and 403.48 ms of the wall clock is in no row read: at load 37
  the fixture waited for a cpu the run did not account for. Not this
  stream's code (the launcher's changes do not touch the triage's rows);
  for stream triage, on a quieter machine.

`cargo clippy --all-targets -- -D warnings` and `cargo fmt --check`, run on their own: clean.
