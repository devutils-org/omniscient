# Stream runner, fifteenth wave — what the run costs, sees and says

Six items, each held to something known apart from the code: a program's own
allocations, the kernel's account of the tool read by `wait4`, a task's own
`getrusage` switch count on the run's clock, a file on disk.

---

## 1. The aligned allocators are hooked — a silent loss closed

`shim/src/lib.rs`. `aligned_alloc`, `memalign`, `valloc` and `pvalloc` are
interposed the way `malloc` is (two helpers, `aligned` and `page_aligned`, each
resolving its symbol once into its own slot), and `posix_memalign` now resolves
once into a slot too, where before it called `dlsym` on every allocation.
glibc serves each of them on its internal allocator, not through `malloc`'s
PLT slot, so none of them had ever been seen: `dd bs=64M` allocates its buffer
with `aligned_alloc` and reported a largest allocation of 4 KiB and no loss.

`reallocarray` was hooked too, and the test showed it counted twice: glibc's
`reallocarray` calls `realloc` through the interposable symbol, so it was
already seen. That hook was taken out, and the test keeps `reallocarray` as the
opposite case. Also fixed in passing: a `realloc` that failed reported the
block as freed, though a failed `realloc` leaves the block where it was. Only
a `realloc` to zero bytes frees without returning another block.

**Ground truth** (`tests/aggregation.rs`):
- `every_aligned_entry_point_is_seen_at_the_size_its_caller_asked_for`: a
  program makes 2, 3, 4, 5, 6 and 7 allocations of 4, 8, 16, 32, 64 and 128 MiB
  through `aligned_alloc`, `memalign`, `valloc`, `pvalloc`, `posix_memalign`
  and `reallocarray`. Each size is alone in its log2 bucket, and each count has
  to be exact. Against the old shim: *"aligned_alloc: 2 of 4194304 bytes made,
  0 counted; memalign: 3 ... 0; valloc: 4 ... 0"*. With the extra hook it had
  said *"reallocarray: 7 ... 14 counted"*.
- `a_program_that_allocates_its_buffer_aligned_shows_that_buffer_as_its_largest`:
  `dd bs=64M count=1`, whose largest bucket has to reach 64 MiB.
- `a_failed_realloc_leaves_its_block_live_where_a_realloc_to_zero_frees_it`:
  a program holds a 256 MiB and a 128 MiB block, asks the first to grow to
  `1 << 62` bytes (glibc refuses anything past `PTRDIFF_MAX`) and the second to
  shrink to zero, and checks both NULLs itself. `mem.heap.live`'s last value
  has to be at least 256 MiB and under 384 MiB. Against the old condition:
  *"live at the end: 411161"*.

Also in `src/collect/mem/heap/mod.rs` (D's two requests): `find_shim` looks
beside the binary and then in `<exe dir>/../lib/omn/`. When the shim is in
neither place, the refusal names both places and says *"it ships beside the
binary, so reinstall, or build it with `make` (or `cargo build -p
omn-preload`) in a checkout, or point OMN_PRELOAD_LIB at it"*. Test:
`the_shim_is_found_beside_the_binary_or_under_lib_omn_and_its_absence_advises_both`,
over a directory tree it creates.

## 2. `cost.json` adds up to the kernel's account

U's finding: a one-second profile with stacks reported 26 ms where the kernel
counted 986 ms of `omn` beyond the target. Measured here with `perf record` on
`omn`: 46 % of its samples are in `KernelSymbols::read`, from 12590.70 s to
12591.64 s. The target's first sample is at 12591.64 s, so this is the
kernel's symbol table, parsed **before** the window opens
(`stack::prime_kernel_symbols`, so that no drain pays for it). The finding
guessed it was symbolizing after the target exits. It is not: the window
excludes startup by design, and `cost.json` said nothing about startup.

`collect::Overhead` gains two fields:
- `before_window_cpu_ns`: this process's `getrusage(RUSAGE_SELF)` at the
  opening of the span `collector_cpu_ns` weighs;
- `after_window_cpu_ns`: from the end of that span to the collection being
  handed back.

`before_window_cpu_ns` is "before this collection's span", not "before the
target ran". For a collection open from the start they are the same thing. For
one attached mid-run (`attaching` resets its `since`), or detached and
reopened, it also holds what the other collections' windows cost before this
one's opened. So `tool_cpu_ns()` is a process total that every collection of a
run carries, up to its own hand-back: the entries of a `cost.json` array are
compared, never summed. The field docs say so.

`Overhead::tool_cpu_ns()` is the sum of the two and `collector_cpu_ns`. The
percentage and the budget are unchanged: none of the startup is dilation (see
"For DECISIONS.md").

**Ground truth**: `budget::what_a_profile_with_stacks_says_it_cost_adds_up_to_what_the_kernel_counted`.
A python target burns half a second and prints its own lifetime
`RUSAGE_SELF`. `wait4` over `omn` minus that figure is the kernel's account of
`omn`, and the three fields together must come to no more than it (plus 5 ms)
and at least four fifths of it. Measured: *"967.9 ms before the window, 82.2
ms in it and 0.7 ms after, 1050.8 ms in all; the kernel counts 1062.5 ms"*. The
old record (82 ms) fails it by a factor of thirteen.

The actual waste is U's: a profile whose callchain is user-only still primes
the kernel's symbols (filed).

## 3. A rate over joules prints in watts

`src/encode.rs`, `per_second_quantity`: a rate over `Unit::Joules` prints as
`Unit::Watts` with no `/s` (*"21.09 W"*); every other unit keeps its `/s`.
Test: `encode::tests::a_rate_over_joules_is_printed_in_watts` (42.18 J over
two seconds reads 21.09 W; a byte rate still ends in `/s`).

## 4. Phase 6 audit: the drain across a budget's move, counted

**(a), landed.** `budget::a_budget_s_move_between_backends_drops_nothing_and_says_where_both_counted`
uses `sched.switches -m count --pid` over a task that sleeps 0.2 ms in a loop
and logs `(CLOCK_MONOTONIC, nvcsw + nivcsw)` around every sleep. At
`--budget 1%` the run moves from `pmu:sw` onto `bpf:raw_tp` (declared `low`)
and then stops, because nothing below that is cheaper. The task's log brackets
what the window held.

The control, the same run at 10000 %, stays on one backend and lands inside
the bracket every time (11985 to 11987, 11986 counted). The moved run counted
**2 over the bracket's top, every time**: a double count at the seam.
`Spending::degrade` starts the replacement before it stops the outgoing
collector, so for about 2 ms both are counting.

The fix keeps the overlap and makes it bounded and said. Stopping first would
trade the overlap for a gap, whose events nobody counts and nothing could
show. So the outgoing collector's `stop` now comes directly after the
replacement's `start`, with nothing in between, and the move's note says
*"both counted from 551.422 ms to 553.452 ms into the run, and what the target
did in that span can be in both"*. The test holds three things:
- `counted + lost >= least`: nothing was dropped at the seam;
- `counted <= most + the task's own switches inside the announced overlap`:
  nothing was counted twice outside it;
- the move is announced.

Three runs: 3966 against 3963..3965 plus at most 9 in the overlap; 5193
against 5190..5192 plus 10; 5281 against 5278..5280 plus 9.

**(b), open.** The ladder trace -> hist -> sample does not exist to be
exercised. `Ladder` (`src/main.rs`) steps between **backends** of one request's
mode, and no catalog entry has backends that trace, aggregate in the kernel
and sample. `Cheaper::next` already returns a whole `ResolvedRequest`, and
`degrade` prepares any mode it is handed. What is missing is a decision
(below), then the mode step in `Ladder::next`, then an entry. Filed.

While at it, measured and not fixed (reported in "Open" below): a launched
target's cpu is read from `/proc/<pid>/stat` in clock ticks. For a target that
burns under a tick per check interval, the budget's percentage stays "not
knowable yet" for the whole run, and the only verdict is the one at the end.
`sched.offcpu --budget 5%` over a sleeping python loop was 16 % over its
ceiling at the end and was never moved.

## 5. Phase 8 audit: the table asserts what it measures

`examples/bench/harness.rs`:
- `Planned.declared` carries the catalog's band.
- `judge_band` holds a row to that band on the larger of its share of the
  target's cpu and its dilation, because an in-kernel collector's cost is in
  the dilation.
- The outcome is `Within`, `Over` or `Missing`.

`tests/bench_backends.rs` gains a `band` column in `table.md` and
`table.json`, and a section listing the flagged rows. A **missing** row (no
completed repetition, no collector cpu, or nothing to hold to a band) fails
the test by name. An **over** row is flagged and said, not failed. Still
`#[ignore]`, and not run here. The rule is checked in `make check` on
synthetic rows (`bench_harness::a_row_is_held_to_its_band_on_the_larger_of_its_share_and_its_dilation`,
`a_row_with_a_column_missing_is_refused_rather_than_judged`).
`docs/bench.md`, "The band column".

## 6. Filed against the runner's files in the fourteenth wave

| Request | Where | Now |
|---|---|---|
| `pub mod net` (K) | `src/collect/mod.rs` | landed; K's `net_probe` shim can go (filed) |
| `ProgramStats::for_run` onto `bpf::enable_run_time_stats` (K) | `src/collect/mod.rs` | landed; the local copy of the command is gone |
| "here" naming a build's gap (P, optional) | `src/render.rs` | landed: *"no backend is usable in this build: this machine serves `bpf:raw_tp`, and no collector is written for it"*. Test `render::tests::here_tells_a_gap_in_this_build_from_one_in_this_machine`, on `power.throttle`'s real entry against `collect::coverage` |
| shim: aligned allocators, refusal wording, `lib/omn/` (D) | `shim/**`, `heap/mod.rs` | landed (item 1) |
| "no viable backend" leads with the passed-over one (D) | the text is in `src/main.rs` | refiled to the owner of `src/main.rs` |
| #20, `emit::stack` on `value_of` | `src/emit.rs` | already fixed on `main` (`measured`, and `unmeasured` counted) |
| two bench tests sharing a report file | `examples/bench/harness.rs` | already fixed (`report.<pid>.<call>.<tag>.json`) |
| skip where the machine withholds the source (P): `runner.rs`, `bench_capabilities.rs`, `collect/mod.rs`'s system-run test, `triage.rs`, `encoders.rs` | runner's tests | already on `main`: `try_resolved`, `proc.threads`, `cpu.util`, and the "has no viable backend" skips |
| `psi` declared where it lives (R and K) | `collect/mod.rs` and K's three files | the coordinator's, in one commit |

## Checks

- While iterating: `cargo test --lib` for `heap`, `encode` and `render`;
  `tests/aggregation.rs`, `tests/budget.rs` and `tests/bench_harness.rs`, and
  the new tests run alone three times each.
- `make check` once: red on
  `budget::the_extra_cpu_a_faster_poll_costs_is_the_extra_cpu_the_shell_observes`
  (tally #14, *"the shell saw 0ns more"*), which passed alone at once, and on
  `family_mem::the_working_set_is_what_was_touched_and_not_what_was_held`
  (in no tally). `make check` stops at its first red binary. Clippy
  (`--workspace --all-targets -D warnings`) and fmt are clean.
- `cargo test --workspace --no-fail-fast` once, at five other streams' load:
  **2072 passed, 8 failed, 8 ignored**. All eight passed on one rerun alone,
  and none of them is in the runner's new code:
  - `scenario_cpu::a_counted_window_is_the_same_number_in_every_format_that_carries_it`
    (K's list of the fourteenth wave);
  - `scenario_mem::both_huge_page_backends_read_the_same_promotion_counter`
    (tally #21), `a_gibibyte_of_reads_costs_one_l1_miss_per_cache_line` and
    `a_walk_that_fits_in_l1_is_never_timed_and_one_that_does_not_waits_on_memory`;
  - `scenario_net::every_datagram_sent_to_a_closed_port_is_a_drop_with_one_reason`
    (P's list; 19779 `SOCKET_RCVBUFF` drops from elsewhere on the machine);
  - `scenario_proc::defect_2_the_last_reading_of_a_launched_target_is_one_the_program_was_alive_for`
    (last reading 0 for 20);
  - `triage::a_verdict_read_off_a_lossy_profile_says_its_amount_is_a_lower_bound`;
  - `unwind::a_stripped_build_is_named_from_debuginfod_only_when_asked_and_as_the_unstripped_one_is`
    (one branch of the call tree was not sampled).

- Review pass: `recorder::a_recording_is_held_to_its_budget_over_a_rolling_span_not_the_whole_run`
  failed once in a run of the stream's own test files (`closed_ns > spun`,
  `tests/recorder.rs:666`). It is already in the flaky tally
  (`docs/open-findings.md`, around line 7304), and it passed alone at once.
  The new realloc test passed, and failed against the old condition.
- Review pass, `make check` once: red on
  `collect::tests::the_runner_closes_the_window` ("it came 9889259 ns early",
  in the fourteenth wave's list for the flaky tally), which passed alone at
  once. Then `cargo test --workspace --no-fail-fast` once, at load 12 to 18 on
  24 cpus: **2078 passed, 3 failed, 8 ignored**. Clippy and fmt clean,
  `target/debug/omn` still carries `cap_bpf,cap_perfmon`. Rerun alone once:
  - `scenario_sched::a_forced_switch_program_splits_the_switches_as_the_kernel_counts_them`
    ("17 involuntary switches of its own and the program 25, with 0 lost"):
    passed.
  - `scenario_mem::both_huge_page_backends_read_the_same_promotion_counter`
    (tally #21, a machine-wide counter other streams move): failed again.
  - `unwind::each_mode_copies_what_it_declares_and_dwarf_costs_more_per_sample_than_fp`
    (in no tally): failed again, *"fp 14.9 us a sample ..., 30.4 us a sample;
    dwarf 26.6 us ..., 32.4 us"*. fp's two runs differ by a factor of two, so
    the band is the machine's load and not a mode. The test measures `omn`'s
    cpu from outside it, and this branch changes nothing on the sampling path
    (its `collect/mod.rs` changes are the budget's move, `cost.json`'s fields
    and `ProgramStats`). Filed for the flaky tally below.

## Review, fifteenth wave

1. The recorder flake: real but not new, see "Checks". Nothing changed.
2. The failed-realloc claim had no test: fixed, with
   `aggregation::a_failed_realloc_leaves_its_block_live_where_a_realloc_to_zero_frees_it`
   (item 1).
3. `before_window_cpu_ns`'s doc said "before the target ran": fixed. The doc
   now says "before this collection's span", and `tool_cpu_ns` says it is a
   process total that is never summed across collections (item 2). The filed
   request for the `cost:` line was reworded to match.

## Landed, each with its test

1. The aligned allocators hooked (`aggregation::every_aligned_entry_point_is_seen_at_the_size_its_caller_asked_for`,
   `aggregation::a_program_that_allocates_its_buffer_aligned_shows_that_buffer_as_its_largest`).
   A failed `realloc` no longer reported as a free
   (`aggregation::a_failed_realloc_leaves_its_block_live_where_a_realloc_to_zero_frees_it`).
   The shim found under `lib/omn/`, and a refusal that advises a reinstall
   (`heap::tests::the_shim_is_found_beside_the_binary_or_under_lib_omn_and_its_absence_advises_both`).
2. `cost.json`'s `before_window_cpu_ns`, `after_window_cpu_ns` and
   `Overhead::tool_cpu_ns`, held to `wait4`
   (`budget::what_a_profile_with_stacks_says_it_cost_adds_up_to_what_the_kernel_counted`).
3. Watts, not `J/s` (`encode::tests::a_rate_over_joules_is_printed_in_watts`).
4. The seam of a budget's move: the overlap made as short as the two calls and
   announced, and the count across it held to the task's own count
   (`budget::a_budget_s_move_between_backends_drops_nothing_and_says_where_both_counted`).
5. The bench table's band column: a missing row fails the test, an over row is
   flagged (`bench_harness::a_row_is_held_to_its_band_on_the_larger_of_its_share_and_its_dilation`,
   `bench_harness::a_row_with_a_column_missing_is_refused_rather_than_judged`).
6. `pub mod net`, `ProgramStats::for_run` on the layer's call, and "here"
   naming a build's gap (`render::tests::here_tells_a_gap_in_this_build_from_one_in_this_machine`).

## Open, with the measured reason

- **Phase 6 (b), trace -> hist -> sample**: no ladder of modes exists
  (`Ladder` walks backends in `src/main.rs`), and no catalog entry has the
  three rungs. It needs the decision below, `main.rs` and `catalog/`. Filed.
- **The budget cannot weigh a target that burns under a clock tick per check**:
  `/proc/<pid>/stat` counts in 10 ms ticks, so `Spending::check` saw observed
  cpu of `Some(0)` at every check of a sleeping python loop (7 checks, 0.2 to
  1.5 ms of tool cpu), and moved nothing, although the run ended 16 % over a
  5 % ceiling. A nanosecond account (`/proc/<pid>/task/*/schedstat`, as
  `tests/unwind.rs` reads) would make the check see it. That changes the
  denominator of every percentage, so it waits on the decision below and was
  not done here.
- **The bench's second table** was not run here, by the brief; the band column
  is exercised only by it.

## Needs privilege

- None to land any of the above. The eBPF half of item 4 needs `CAP_BPF` on
  `target/debug/omn`, which `make` granted in this worktree through the
  sudoers line in `CLAUDE.md`. Without it the test skips and says so.
- To let `cost.json` measure the eBPF programs' time inside the target (it
  reports that as unknown on this machine, `kernel.bpf_stats_enabled` = 0), a
  human runs `sudo sysctl kernel.bpf_stats_enabled=1` before the run. Optional,
  and nothing here depends on it.

## For `DECISIONS.md`

- **What a run cost the machine is reported whole, and what it cost the target
  is weighed apart.** `cost.json` carries this process's cpu before the window
  (startup, probes, the tables the run reads first, such as the kernel's
  symbols) and after it, beside the window's. The percentage and `--budget`
  stay over the window: none of the rest dilated the target.
- **A budget's move overlaps the two backends rather than leaving a gap
  between them, and says where.** A gap's events are counted by nobody and
  nothing could show them; an overlap is bounded by the two calls and
  announced, so a reader knows which span may be in both.
- **A table row over its declared band is flagged, not failed; a row missing a
  column fails the table.** A band a machine does not keep is a claim to
  correct; a row with nothing measured is a table that is wrong.
- **To decide: what one series made of two modes means to an emit.** This is
  what the trace -> hist -> sample ladder needs before it is built. Proposed,
  in line with "an invalid combination is refused, never degraded silently": a
  budget may move only onto a mode whose events the request's emits can take
  (a `hist` of traced events and an in-kernel `hist` merge; a `raw` does not
  survive the move and is refused up front by the budget note).
- **To decide: the budget's denominator in nanoseconds.** Proposed: a launched
  or pid target's cpu from `schedstat` (nanoseconds, on-cpu), not `stat`
  (ticks).

## Filed for other streams

Under "Needs from another stream — the fifteenth wave, stream runner" at the
end of `docs/open-findings.md`:
- the owner of `src/main.rs`: the "no viable backend" refusal leading with the
  backend passed over; the `cost:` line naming the cpu before the window; the
  ladder's own `force:` pin reported as the user's; the mode ladder (with
  `catalog/`);
- U: `profile.rs` primes the kernel's symbols for a user-only callchain;
- K: drop `collect::mem::net_probe` now that `collect::net` is public;
- the owners of `tests/family_mem.rs`, `tests/scenario_mem.rs` and
  `tests/cross_memory.rs`: a shim check whose string never matches;
- the owners of `examples/gpu-draw-in-watts.yml`, `examples/power-energy.yml`
  and `docs/scenarios/power.md`: `J/s` in quoted outputs, now `W`.

For the ROADMAP (the coordinator's file): phase 6's second criterion can read
*"the drain across a move counted against a task's own count, and the seam
announced; the mode ladder not built"*. Phase 8's first can add *"each row held
to its declared band by the table test"*.

For the flaky tally: `budget::the_extra_cpu_a_faster_poll_costs_is_the_extra_cpu_the_shell_observes`
(#14) once more (*"the shell saw 0ns more cpu in the faster run and the tool
reported 206405000ns more"*), and
`family_mem::the_working_set_is_what_was_touched_and_not_what_was_held` once
(*"its busiest window came back as 232497152 against a resident set of
278454272"*), in no tally; and the eight failures listed under "Checks",
from the `--no-fail-fast` run, each of which passed alone:
`scenario_proc::defect_2_...` (*"left: 0, right: 20"*), `scenario_mem`'s two
cache tests under THP `always` (*"the TLB counter saw 103138 misses"*, *"8 of
29 of the small walk's loads did"*), and `unwind::a_stripped_build_...` are in
no tally.

- For the flaky tally (not this stream's files):
  `unwind::each_mode_copies_what_it_declares_and_dwarf_costs_more_per_sample_than_fp`
  (`tests/unwind.rs:873`) failed twice in a row at load 12 to 18, with fp's
  two runs 14.9 and 30.4 us a sample: a separation the load erases. Worth
  holding under a quiet machine, or comparing per-sample cost within one
  interleaved run.

## Session index rows

None: this stream added no session.
