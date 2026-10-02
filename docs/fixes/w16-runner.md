# Stream runner, sixteenth wave: the ladder across modes, the denominator, the elsewhere

Five items. Each new test is held to something known apart from the code: a
task's own `getrusage` count of its faults on the run's clock, a child's own
process and thread clocks, files `std::fs` says exist or do not, and the
collector registry `omn` itself refuses by.

---

## 1. The two runner proposals, settled and built

### (a) What one series made of two modes means to an emit

Settled by alignment with "an invalid combination is refused with the reason,
never degraded silently" and "every mechanism as a chain":

- `src/request.rs`: `EmitKind::weighs_each_event` (`hist`, `topk`, `rate`,
  `summary`: the emits that read an event's `weight`), `mode_move(from, to,
  emits) -> Result<(), ModeMove>`, and `Mode::cheaper()`. The only rung is
  `trace` -> `sample` at the default period. `count` closes a window into one
  total, which no emit of events merges with. `poll` and `snapshot` read a
  value, not an event. The in-kernel histogram is not a rung: a collector
  takes it at the start whenever the request's emits allow (`io.latency` with
  `hist` alone), and stepping onto it mid-run would mean dropping every emit a
  histogram cannot answer.
- `src/main.rs`: `rungs()` orders what a budget can step onto. First the
  backends below the serving one, in the same mode, whose band fits; each of
  these keeps every event. Then the cheaper mode, on the serving backend and on
  those below it that fit, when `mode_move` allows it. A mode the emits cannot
  be produced in (`summary` under `sample`) takes that rung out, and
  `budget_note` then says the run would stop. An emit the cheaper mode does
  produce, but only by putting two kinds of event in one list (`raw`,
  `timeline`, `stack`), is refused before the run with exit 2
  (`budget_refusal`):
  *"[mem.faults] --budget 5% may move this collection from `trace` to
  `sample@99hz` on `pmu:sw`: `raw` would hold the events of both modes as one
  kind, though a traced event is one occurrence and a sampled one stands for a
  period. Drop `raw` or the budget, or ask for `-m sample` from the start"*.
- `Ladder::next` walks the rungs. A move between modes adds a note to the
  results: *"the budget moved this collection from `trace` to `sample@99hz` on
  `pmu:sw`: ... after it each stands for its period, which the emits weigh by
  the event's `weight`"*. `Ladder::record` finds the serving request by the
  number of moves the collection made, not by backend id, because a mode move
  keeps the backend. `cost.json`'s `mode` reads `trace then sample@99hz`.
- `assemble` reads the events. When a series moved modes and no event carries
  a weight, it says the totals are short after the move
  (`unweighed_after_a_mode_move`), instead of claiming the weighing.

**Ground truth**, `budget::a_traced_series_the_budget_moves_onto_sampling_counts_what_the_task_did`.
`mem.faults` is the one entry whose backend (`pmu:sw`) both traces and
samples. A python task faults 1024 fresh pages a round (`MADV_NOHUGEPAGE`,
then `MADV_DONTNEED`) and logs `(CLOCK_MONOTONIC, ru_minflt + ru_majflt)`. At
`--budget 5%` the run is over at its first check (about 88 % traced). It moves
to `sample@99hz` and holds there. The test checks:

- both announcements are present, and no "was pinned" note is (item 2);
- the trace half drops nothing: the events counted are at least the task's
  faults before the announced overlap (*45057 before the overlap, 63431
  counted*);
- the weighed half: the series' weighed count is the task's own count, short
  by at most a period per cpu and over by at most what the overlap held.

That last check needs the sampled faults to carry `weight = period`, which is
`page_faults.rs` and the mem stream's file. It skips loudly and names the
request until that lands. With the two-line diff applied locally and then
reverted, three runs counted 1474515 against the task's 1474603..1476651,
1583251 against 1583151..1585199, and 1657423 against 1656881..1658929. Without
it the count is about 63 000 against 1 650 000, and the run says so.

`budget::a_budget_that_could_move_a_raw_list_between_modes_is_refused_up_front`
holds the refusal against the validator's own matrix. `-m trace -e raw` and
`-m sample -e raw` are each valid alone (exit 0), and `-m sample -e summary` is
not (exit 2). So the exit 2 for `trace` + `raw` + `--budget` is about the move.
`summary` passes and is told the run would stop; `hist` is told the rung
(`` `pmu:sw` in `sample@99hz` ``).
`request::tests::a_series_crosses_modes_only_in_the_emits_that_weigh_each_event`
holds the rule against the mode x emit matrix.

### (b) The budget's denominator in nanoseconds

`src/collect/task.rs`: `process_cpu(pid)` and `thread_cpu(tid) -> CpuRead {
ns, grain }`.

- A process is read from its process cpu clock (`clock_getcpuclockid`). That
  is the scheduler's `sum_exec_runtime` summed over every thread, the exited
  ones included, which a sum of `schedstat` files would miss. The reaped
  children's `cutime`/`cstime` are added, in ticks, because no finer account of
  them exists.
- A thread is read from `/proc/<tid>/task/<tid>/schedstat`.
- Where neither can be read (no `CONFIG_SCHED_INFO`, another pid namespace),
  the reading falls back to `stat`'s ticks and says so (`CpuGrain::says`).

Held by `collect::task::tests::a_target_that_burns_under_a_tick_is_read_in_nanoseconds_against_its_own_clock`.
A python child burns 3 ms and reports its own process and thread clocks before
and after the read. Both readings fall between the child's figures, and `stat`
reads a whole number of ticks.

The meter that weighs the budget is `Meter` in `src/collect/mod.rs`, which is
not this stream's file. The swap is filed with the exact lines.
`budget::a_target_that_burns_under_a_tick_per_check_is_weighed_and_the_budget_acts_on_it`
is written and `#[ignore]`d until that swap lands. The task is a python loop
that sleeps 1 ms at a time and logs its own process clock. The tool runs
`mem.rss -m poll@1ms --budget 5%`.

- **Without the swap:** *"weighed the task at 0 ns and the task's clock puts
  14388704 to 14403814 ns inside the window, which lasted 3000 ms; served by
  1 backend(s), stopped because the requested duration elapsed"*. This is the
  16 % case: the ceiling was never acted on.
- **With the swap** (applied locally and reverted), it passed twice: *"weighed
  the task at 1190171 ns and the task's clock puts 1181471 to 1234731 ns ...
  served by 2 backend(s), stopped because `mem.rss` spent 378.09 % ... since it
  was moved onto `fs:proc:statm`"*.

## 2. `src/main.rs`: the `cost:` line, and the ladder's pin

- `MeasuredCost::line` names the cpu spent before the window when it outweighs
  the window's: *"; and 967.9ms before this collection's window opened
  (startup, probes, the tables the run reads first), which is in no percentage
  here"*. `budget::what_a_profile_with_stacks_says_it_cost_adds_up_to_what_the_kernel_counted`
  now holds the line's figure to `cost.json`'s and to the kernel's account
  from `wait4`, and checks that the line stays silent when the window
  outweighs it.
- `resolve::resolve_onto(catalog, capabilities, request, backend)` resolves
  onto a backend with every check a pin gets, and marks nothing pinned. The
  ladder uses it, so a budget's move no longer reports *"backend `bpf:raw_tp`
  was pinned"*. Held by the mode-ladder test above, which asserts that no
  pinned note is present.

## 3. `src/resolve.rs`: `FilterFieldNotCarried` names its elsewhere for what keeps the request from it

`elsewhere` is now `Option<Box<KeyElsewhere>>`, picked the way a key's is:
the first backend of the best kind among those that carry the field, so the
order is forceable, then missing, then unwritten, then unfit. The pin hint is
given only for a backend this build collects and this machine can run.
`resolution_is` (`src/main.rs`) and both of `src/triage/engine.rs`'s matches
take its `whose()`. The old code named the first carrier with `--backend
force:` whatever it was.

Test: `resolve::tests::a_filter_field_another_backend_carries_is_named_for_what_keeps_the_request_from_it`.
It uses a catalog fixture: `io.latency` with its `rwbs` carriers rewritten in
three ways.

- Every carrier needs a path that does not exist (checked with `std::fs`):
  the answer is `Missing`, whose `Machine`, with no pin hint.
- Every carrier is renamed to a tag nothing collects (checked against
  `collect::implements`): the answer is `Unwritten`, whose `Build`.
- A collected carrier needs only `/proc/diskstats`: the answer is `Forceable`,
  and the message names `--backend force:bpf:raw_tp`.

## 4. The bench: the guardrail, and the table that fails

- **The guardrail stays the harness's own `thermal_throttle` read**, with the
  sentence on `throttle_count` and in `docs/bench.md`. The guardrail wraps the
  bare runs too, and the bare run is the dilation's baseline: an `omn` beside
  it would turn the baseline into a traced run.
- `harness::table_failures` holds the whole table. It fails when the table has
  no row, when a planned pair is neither a row nor listed as excluded by the
  gate, when a row was served by another backend than the one it forced, and
  when a row is missing a column. An over-band row is still flagged and not
  failed. `tests/bench_backends.rs` calls it.
  `bench_harness::a_wrong_backends_table_fails_and_a_row_over_its_band_alone_does_not`
  holds it on synthetic tables in `make check`. `scripts/bench.sh backends` is
  the second table. It is not run here.

## 5. Filed against the runner's files in wave 15

| Request | Now |
|---|---|
| "no viable backend" leads with the backend passed over (`main.rs`) | already on `main` (`refused_at_collect`, `PassedOver`) |
| the `cost:` line and the cpu before the window | landed (item 2) |
| the ladder's pin is not the user's | landed (item 2, `resolve_onto`) |
| phase 6's trace -> hist -> sample ladder | landed as trace -> sample; the hist rung is taken at the start (item 1a) |
| the swap to `join::decompose_within`, `check_paired_parts` gone (`main.rs`, `triage/engine.rs`, `request.rs`) | already on `main` |
| `NoBackendDeclares`, `KeyNotCarried`, `StackRefused` arms in `triage/engine.rs` | already on `main`; `FilterFieldNotCarried` added to both matches |
| `proc` has no producer (`request.rs` and `event.rs`) | not landed: removing `KeyKind::Proc` needs `KeyValue::Proc` in `src/event.rs` and `docs/keys.md` in the same commit, and a producer belongs to the procfs collectors; neither file is this stream's |

## Checks

- While iterating: `cargo test --lib` for `resolve`, `request`,
  `collect::task`; `tests/budget.rs` (new tests run three times each),
  `tests/bench_harness.rs`, `tests/bench_backends.rs` (compiled; the table is
  `#[ignore]`), `cargo test --bin omn`.
- The machine's disk filled twice during the wave (another stream's files):
  `rustc` and `lld` died with SIGSEGV and SIGBUS mid-link. This worktree's
  `target/debug/incremental` was removed, and it builds with
  `CARGO_INCREMENTAL=0`.
- `make check` once: see the commit's message for its result.

## Landed, each with its test

1. The mode rule and the ladder across modes, with the refusal up front:
   `budget::a_traced_series_the_budget_moves_onto_sampling_counts_what_the_task_did`,
   `budget::a_budget_that_could_move_a_raw_list_between_modes_is_refused_up_front`,
   `request::tests::a_series_crosses_modes_only_in_the_emits_that_weigh_each_event`.
2. The nanosecond readers:
   `collect::task::tests::a_target_that_burns_under_a_tick_is_read_in_nanoseconds_against_its_own_clock`;
   the budget's use of them is written,
   `budget::a_target_that_burns_under_a_tick_per_check_is_weighed_and_the_budget_acts_on_it`
   (`#[ignore]` until the `Meter` swap).
3. The `cost:` line before the window:
   `budget::what_a_profile_with_stacks_says_it_cost_adds_up_to_what_the_kernel_counted`.
4. The ladder's move is not a pin (`resolve_onto`): asserted in the mode
   ladder test.
5. `FilterFieldNotCarried`'s elsewhere:
   `resolve::tests::a_filter_field_another_backend_carries_is_named_for_what_keeps_the_request_from_it`.
6. The bench table check and the guardrail's reason:
   `bench_harness::a_wrong_backends_table_fails_and_a_row_over_its_band_alone_does_not`.
7. `man/omn.1` and `--help` say the ladder steps from trace to sample and
   which emits survive.

## CI expectation

None of this stream's tests failed on 37f17c8's two cells: `aggregation`,
`bench_*`, `budget`, `encoders`, `recorder`, `runner` and `triage` all passed on
both. The cells' failures are in other streams' files. The new and changed
tests:

| Test | defaults (paranoid 2, no caps) | stock (paranoid 4) |
|---|---|---|
| `resolve::...a_filter_field_another_backend_carries...` | pass: a fixture, path probes and the registry, no perf | pass, the same |
| `request::...a_series_crosses_modes...` | pass: pure | pass |
| `collect::task::...burns_under_a_tick...` | pass: a process clock of one's own child needs no privilege; `schedstat` is 0444 (a kernel without it takes the thread half's own skip) | pass, the same |
| `budget::a_traced_series...` | `SKIP: mem.faults is not served here` (the cell refused `mem.faults` at start in 37f17c8's log, EACCES); if served, user-mode faults are the task's own, and it passes, skipping the weighed half | `SKIP: mem.faults is not served here` (no viable backend at paranoid 4) |
| `budget::a_budget_that_could_move_a_raw_list...` | pass: `--dump-request` on the test's own pid resolves `pmu:sw` there (the same log resolved it) | `SKIP` (no viable backend) |
| `budget::what_a_profile_with_stacks...` | as before (passed); the new line check runs where `cpu.profile` is served | as before (`SKIP`: no `cpu.profile`) |
| `budget::a_target_that_burns_under_a_tick_per_check...` | ignored | ignored |
| `bench_harness::a_wrong_backends_table...` | pass: synthetic | pass |

The `SKIP:` lines are the file's existing convention, and the skip gate reads
only `skipped:` and `SKIPPED`, so none of them can fail a cell.

## Open, with the measured reason

- **The weighed half of the mode ladder** waits on `page_faults.rs` carrying
  `weight = period` on sampled faults (filed). That is a defect of its own as
  well: `mem.faults -m sample@N` counts each sampled fault once.
- **The denominator in the budget** waits on the `Meter` swap in
  `src/collect/mod.rs` (filed, two lines and the descriptions). The readers
  and the test are landed.
- **A move between modes is called "two backends"** in `Spending::degrade`'s
  loss note and "moved onto `pmu:sw`" in the check's note (filed). The
  ladder's own note in the results names both modes.
- **The bench's second table** was not run, per the brief.
- **`proc` has no producer**: see item 5.

## Needs privilege

None. Everything above ran as uid 1000 with what `make` grants
(`cap_bpf,cap_perfmon` on `target/debug/omn`, verified with `getcap`).

## For DECISIONS.md

Move the three "Open" lines to the settled ones:

- **What one series made of two modes means to an emit.** A budget may
  continue a series in another mode only for the emits that weigh each event
  by what it stands for: `hist`, `topk`, `rate`, `summary`, which read
  `weight`. A traced event weighs one, an in-kernel histogram row its bucket's
  count, and a sampled event its period. A mode that cannot produce an emit
  the request asked for is not a rung, as a backend that cannot serve the
  target is not. An emit the cheaper mode produces only by listing two kinds
  of event as one (`raw`, `timeline`, `stack`) makes the request invalid under
  a budget, and it is refused before the run, exit 2. The only mode rung is
  `trace` -> `sample`, taken after the same-mode backends below. The in-kernel
  histogram is taken at the start when the emits allow it, never stepped onto,
  because the step would drop the emits a histogram cannot answer.
- **The budget's denominator is the scheduler's nanoseconds.** For a process,
  its process cpu clock: every thread's `sum_exec_runtime`, exited ones
  included, plus its reaped children's ticks. For a thread, its `schedstat`.
  `stat`'s ticks are used only where neither can be read, and the record says
  so. (Landing in `Meter` is filed.)
- **The bench's thermal guardrail is the harness's direct read of
  `thermal_throttle`, not `power.throttle` through the tool.** It wraps the
  bare baseline runs, which an `omn` beside them would turn into traced runs,
  and a guardrail measured by the tool under test would pass the tool's own
  defect in that source.
- **A filter field's elsewhere is chosen as a key's is**: forceable, then
  missing, then unwritten, then unfit. A `--backend force:` hint is given only
  for a backend this build collects and this machine runs.
- **A budget's move is not a pin.** The ladder resolves onto its rung without
  marking it pinned, and only the user's `--backend force:` says "the chain
  was not walked".

## Filed for other streams

Under "Needs from another stream — the sixteenth wave, stream runner" at the
end of `docs/open-findings.md`:

- the owner of `src/collect/mod.rs`: the `Meter` swap onto
  `task::process_cpu` / `task::thread_cpu`, then un-ignore the budget test and
  drop the `#[allow(dead_code)]`s; and a mode move named as one in
  `degrade`'s notes;
- the owner of `src/collect/mem/page_faults.rs`: `weight = period` on sampled
  faults;
- the coordinator: the `DECISIONS.md`, `ROADMAP.md` (phases 6 and 9) and
  `STATE.md` lines.

## Session index rows

None: this stream added no session.
