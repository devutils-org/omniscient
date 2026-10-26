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
- **A mode rung is taken only on a collector that weighs its sampled events**
  (`WEIGHS_ITS_SAMPLES`, `weighs_its_samples`, `src/main.rs`; added after the
  review). A sampled event that carries no `weight` counts once where it stood
  for its period, so a series moved onto it is short by that factor after the
  move: a known-wrong combination, which is passed over rather than taken and
  flagged after the fact. No collector of this build weighs its samples yet
  (`mem.faults` on `pmu:sw` is filed), so **in this build the ladder never
  changes mode**: the rung is named as passed over, with the reason, in the
  budget's note (*"The ladder's cheaper mode, `pmu:sw` in `sample@99hz`, is
  passed over: its sampled events carry no `weight`, ..."*) and beside the
  stop when the budget ends the run with nowhere to go. `budget_refusal`
  refuses `raw` only when the rung could be taken (`budget_refusal_given`),
  since a move that cannot happen cannot mix two kinds of event.
- `assemble` still reads the events as a backstop. When a series moved modes
  and no event carries a weight, it says the totals are short after the move
  (`unweighed_after_a_mode_move`), instead of claiming the weighing.

**Ground truth**, `budget::a_traced_series_over_its_ceiling_counts_what_the_task_did_and_moves_only_onto_weighed_samples`
(renamed from `a_traced_series_the_budget_moves_onto_sampling_counts_what_the_task_did`).
`mem.faults` is the one entry whose backend (`pmu:sw`) both traces and
samples. A python task faults 1024 fresh pages a round (`MADV_NOHUGEPAGE`,
then `MADV_DONTNEED`) and logs `(CLOCK_MONOTONIC, ru_minflt + ru_majflt)`. At
`--budget 5%` the run is over at its first check (about 90 % traced).

- **As the build is** (the rung passed over): the run stops at the check,
  the series is traced end to end, and every fault the window held is an
  event: *"the series counts 44706 faults and lost 0 records; the task took
  43008 to 45056 inside the window; mode trace, stopped because `mem.faults`
  spent 91.56 % ... nothing cheaper than `pmu:sw` is available"*. The test
  holds `counted + lost >= least` and `counted <= most`, that nothing is
  flagged after the fact, and that the passed-over rung is named beside the
  stop.
- **Once `page_faults.rs` weighs and `WEIGHS_ITS_SAMPLES` names it**: the
  run moves to `sample@99hz`, and the same test holds both announcements, no
  "was pinned" note (item 2), and the weighed series against the task's own
  count, short by at most a period per cpu and over by at most what the
  overlap held. Before the review, with the two-line `page_faults.rs` diff
  applied locally and then reverted, three moved runs counted 1474515 against
  the task's 1474603..1476651, 1583251 against 1583151..1585199, and 1657423
  against 1656881..1658929. Without it the moved series counted 61892 against
  1537069..1539117 (the reviewer's run): the 25x short total that made the
  rung a known-wrong combination.

`budget::a_budget_takes_no_mode_rung_whose_sampled_events_would_not_be_weighed`
(renamed from `a_budget_that_could_move_a_raw_list_between_modes_is_refused_up_front`)
holds the dumps against the validator's own matrix. `-m trace -e raw` and `-m
sample -e raw` are each valid alone (exit 0), and `-m sample -e summary` is
not (exit 2). Under a budget, `raw` and `summary` are let through and told the
run would stop; `hist` is told the rung is passed over and why.
`main.rs`'s `tests::a_sampled_rung_whose_events_are_not_weighed_is_passed_over_not_taken`
resolves `mem.faults` on this machine and holds the refusal of `raw` (exit 2's
sentence) for a collector that weighs, and the pass-over for the one there is.
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
**So item 1b is not in effect**: the readers are landed and unit-tested,
`process_cpu` / `thread_cpu` are dead code in the binary (three
`#[allow(dead_code)]`), the budget still weighs `stat` ticks, and the 16 %
case is not held anywhere on `main` until the swap lands.
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
| phase 6's trace -> hist -> sample ladder | not landed as asked: the rule and a trace -> sample rung are built, the rung is passed over until a collector weighs its samples, and no hist rung exists (item 1a; see "Review") |
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
- `make check` once: the lib's 1387 tests and every binary up to
  `tests/cli.rs` green (`budget` 21 passed, 1 ignored); `cli` red on *"Disk
  quota exceeded"* writing a copy of the binary. Then one `cargo test
  --workspace --no-fail-fast`: **2104 passed, 29 failed, 11 ignored**, none
  in this stream's files (`budget`, `runner`, `triage`, `recorder`,
  `encoders`, `aggregation`, `cli`, `bench_*` all green). 22 of the 29 are
  `scenario_io` on *"No space left on device"*; the two `join_engine` ones
  (`a_join_that_evicts_...`, `a_new_boundary_is_a_rule_...`) passed alone at
  once; the rest are machine-state scenarios of other streams
  (`scenario_mem`'s THP and reclaim, `scenario_net`'s `NO_SOCKET` drops,
  `scenario_proc::defect_1_...`, `scenario_sched`'s forced-switch split).
  Clippy (`--all-targets -D warnings`) and fmt clean. The worktree's `target`
  was then `cargo clean`ed to let git write the commit (the disk was at 0):
  run `make` before using `target/debug/omn` here again.

### After the review

- `make check` could not run: `/home` reached 92 KB free and the `/tmp`
  tmpfs its user quota (EDQUOT) mid-link, both filled by the six worktrees'
  `target`s (this one's was moved to the scratch tmpfs and its unneeded test
  binaries removed to build at all). Run instead, after `make` (capabilities
  granted, `getcap` checked): `cargo test --bin omn` (4 passed),
  `--test budget` (21 passed, 1 ignored), `--test runner`, `--test cli` (all
  green); `cargo clippy --workspace --all-targets -D warnings` and `cargo fmt
  --check` clean. The changes are in `src/main.rs`, `src/cli.rs`'s help text,
  `tests/budget.rs` and documents, no shared layer.
- In the first combined `budget` run two tests failed under load:
  `the_extra_cpu_a_faster_poll_costs_is_the_extra_cpu_the_shell_observes`
  (in `docs/flaky.md`; passed alone once) and the renamed ladder test, on
  *"too few faults to judge"*: the run stops at its first check, and a loaded
  machine fitted fewer than 20 000 faults into that tenth of a second. Its
  traced branch now asks for two whole rounds of 1024 inside the window, which
  is still an exact bracket; it passed five times alone and once in the file.
- **The coordinator's merge should run the full suite**, which this pass
  could not.

## Landed, each with its test

1. The mode rule (`mode_move`), and a ladder across modes that takes a mode
   rung only on a collector that weighs its samples -- in this build, none,
   so the rung is passed over and named:
   `budget::a_traced_series_over_its_ceiling_counts_what_the_task_did_and_moves_only_onto_weighed_samples`,
   `budget::a_budget_takes_no_mode_rung_whose_sampled_events_would_not_be_weighed`,
   `omn` (bin) `tests::a_sampled_rung_whose_events_are_not_weighed_is_passed_over_not_taken`,
   `request::tests::a_series_crosses_modes_only_in_the_emits_that_weigh_each_event`.
   **Not landed**: phase 6's trace -> hist -> sample ladder end to end (no
   catalog source has the three, and the hist rung is not built), and a
   trace -> sample move held to a workload across the move (waits on
   `page_faults.rs`).
2. The nanosecond readers only:
   `collect::task::tests::a_target_that_burns_under_a_tick_is_read_in_nanoseconds_against_its_own_clock`.
   **The denominator itself is not landed**: the budget's `Meter` still reads
   ticks, and
   `budget::a_target_that_burns_under_a_tick_per_check_is_weighed_and_the_budget_acts_on_it`
   is `#[ignore]` until the filed `Meter` swap.
3. The `cost:` line before the window:
   `budget::what_a_profile_with_stacks_says_it_cost_adds_up_to_what_the_kernel_counted`.
4. The ladder's move is not a pin (`resolve_onto`): asserted on the
   backend move `pmu:sw` -> `bpf:raw_tp`
   (`budget::a_budget_s_move_between_backends_drops_nothing_and_says_where_both_counted`,
   assertion added after the review), and in the mode ladder test's moved
   branch once a mode move can be taken.
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
| `budget::a_traced_series_over_its_ceiling...` | `SKIP: mem.faults is not served here` (the cell refused `mem.faults` at start in 37f17c8's log, EACCES); if served, user-mode faults are the task's own and the traced branch passes | `SKIP: mem.faults is not served here` (no viable backend at paranoid 4) |
| `budget::a_budget_takes_no_mode_rung...` | pass: `--dump-request` on the test's own pid resolves `pmu:sw` there (the same log resolved it); the notes are the catalog's and the rung table's | `SKIP` (no viable backend) |
| `omn` (bin) `tests::a_sampled_rung...` | pass if `pmu:sw` resolves traced, as on the dump; otherwise its `SKIP:` line | `SKIP:` (`mem.faults` traced not served) |
| `budget::what_a_profile_with_stacks...` | as before (passed); the new line check runs where `cpu.profile` is served | as before (`SKIP`: no `cpu.profile`) |
| `budget::a_target_that_burns_under_a_tick_per_check...` | ignored | ignored |
| `bench_harness::a_wrong_backends_table...` | pass: synthetic | pass |

The `SKIP:` lines are the file's existing convention, and the skip gate reads
only `skipped:` and `SKIPPED`, so none of them can fail a cell.

## Open, with the measured reason

- **The mode ladder never changes mode in this build.** The trace -> sample
  rung waits on `page_faults.rs` carrying `weight = period` on sampled faults
  (filed), then one line in `WEIGHS_ITS_SAMPLES`. That is a defect of its own
  as well: `mem.faults -m sample@N` counts each sampled fault once. The hist
  rung of phase 6 is not built, and no catalog source declares trace,
  in-kernel hist and sample, so an end-to-end ladder test cannot be written
  against today's catalog.
- **The ladder is exercised only where `mem.faults` is served**: both CI
  cells skip it (EACCES at paranoid 2, no viable backend at paranoid 4).
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

## For docs/DECISIONS.md

Move the three "Open" lines to the settled ones:

- **A mode rung is taken only on a collector that weighs its sampled
  events.** One that does not would make every total after the move short by
  the period, a known-wrong combination: it is passed over and named, not
  taken and flagged afterwards. Which collectors weigh is the build's fact
  (`WEIGHS_ITS_SAMPLES` in `src/main.rs`, to move into the collector registry
  when its owner adds a field for it).
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
- the coordinator: the `docs/DECISIONS.md`, `docs/ROADMAP.md` (phases 6 and 9) and
  `docs/dev/STATE.md` lines (corrected after the review: the mode ladder is built and
  passes over its only rung; the denominator is not in effect).

## Review

1. **The mode rung undercounting about 25x** -- real. Fixed: the rung is
   passed over on a collector that does not weigh its samples, named in the
   budget's note and beside the stop, and `raw` is refused only when the rung
   could be taken. Tests above.
2. **Item 1b not in effect** -- real. Nothing to change in owned files (the
   `Meter` is `src/collect/mod.rs`); the record now says readers landed and
   the denominator not.
3. **No trace -> hist -> sample ladder end to end** -- real as a deviation
   from the brief. After fix 1 the ladder takes no mode rung at all in this
   build; recorded as not landed, with why (no source has the three; the hist
   rung is taken at start, not stepped onto).
4. **Test run note** -- no defect. The two `bench_harness` failures under
   EDQUOT are the machine's disk (`/home` had 3.1 GB left at the start of
   this pass).

## Session index rows

None: this stream added no session.
