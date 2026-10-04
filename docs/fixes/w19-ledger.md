# The nineteenth wave, stream ledger

Branch `feat/w19-ledger`, from `main` at `91a0cef`. `BRIEF.md`, "ledger --
what a process costs": one process's cost sheet across every resource `omn`
measures, each line with its losses and trust, and the sources that could
not be served said in place. Owned: `view/src/views/ledger.rs`, its
snapshots under `docs/view/gallery/ledger--*`, and `docs/view/ledger.md`.

## What landed

- **`view/src/views/ledger.rs`**, replacing the stub. One line per resource
  -- cpu time, memory held, memory faulted, I/O syscalls, network (added in
  the review round), retransmits, GPU
  memory, GPU time, energy -- in six columns: amount, rate, a shape over the
  run (sparkline, or the syscalls' log2 latency distribution), what knowing
  the line cost `omn` (its collectors' `attributed_cpu_ns` plus
  `in_target_cpu_ns`, `≥` where that is unknown), and the core trust badges,
  the amount and rate dimmed by the same rule. Under a line: per-task shares
  when several tasks hold it; the target's own account and the machine's busy
  time under cpu; an energy estimate by cpu share, in italics and said to be
  an estimate; the user/system split and the bytes read and written when raw
  events carry them. The selected line's detail (backend, trust in
  sentences, the fallback's words, loss reasons, what knowing it cost and the
  runner's words for where that comes from) under the sheet. The sheet's last
  line: what knowing all of it cost, as a share of what the target spent.
- **Refusals and absences in place**: a refused source's line is its reason;
  a source the run did not ask is "not in this run"; under a focus, a line
  with nothing of the focused task names the keys its series carry instead
  of drawing a zero; a run with no target is titled "the machine" and gets no
  own account and no estimate.
- **Live**: every figure is computed from `raw` events when the aggregated
  emits are absent, with the same sums (a kernel row's `total_ns` taken over
  `lat_ns x weight`, as `emit.rs` does); the cost column says "at the end".
- **`--against`**: the rate column becomes the second run's amount and its
  ratio, coloured with `trust::difference_colour`.
- **Keys**: `↑`/`↓` select a line, `t` steps the focus through the tasks,
  `enter` opens the view behind the line (`Action::Goto` with the focus).
- **Snapshots**: the seven `ledger--<fixture>` pairs re-blessed; two more
  drawn by the view's own unit test, `ledger--against--cpython-hot-heavier`
  (the comparison) and `ledger--focus--http-server` (one task, the I/O
  line's detail open).
- **`docs/view/ledger.md`**: what the sheet draws, what each fixture shows,
  what it does not do, and the verdict.

## Shown versus reasoned

Shown, by tests in `ledger.rs` (11) and `tests/view_snapshots.rs ledger`:

- the cpu line equals the sum of the timeline's polls on `cpython-hot`, and
  the heavier fixture's is more than 1.5x the lighter's (×2.06 measured);
- the I/O line's time equals the histogram's exact sum on `sqlite-commits`,
  and its distribution starts at the lowest bucket (512 ns);
- several tasks' faults are split per task on `http-server`, one task's are
  not on `cpython-hot`; a focus on one task lowers the per-task lines and
  leaves the machine's unchanged;
- a refused `gpu.kernel` is drawn with the refusal's words;
- the energy estimate is machine joules x target cpu / machine busy cpu, and
  says "not a measurement";
- a synthetic live run is drawn from raw events: the cpu total, the
  user/system split, the syscall rows' exact sums and bytes, "so far", and
  the cost said at the end;
- the delta, level and sparkline arithmetic, and `t` stepping through tasks.

Reasoned, not shown:

- **The GPU time line on real data.** `gpu.kernel` is refused on every
  fixture (they predate the cupti stream); the line is exercised by nothing
  real. Its per-kernel split reads `kernel` and `duration_ns` from
  `src/collect/gpu/kernel.rs`'s events by name.
- **The live sheet on a real run.** `feed::as_live_until` keeps only the
  `raw` outputs a recording has, and the ledger's sources were recorded as
  aggregates, so no real live snapshot can be drawn from the fixtures; the
  live path is held by the synthetic test above, with attribute names read
  from the collectors (`oncpu.rs`: `ns`, `user_ns`, `system_ns`;
  `histogram.rs`: `count`, `weight`, `total_ns`, `bytes`; `vfs.rs`: `op`).
- That the `http-server` task taking 68 % of the faults is the client or a
  server thread: the summaries key by tid with no command, so the doc says
  neither.

## What I could not do, and why

- **The user/system split and the bytes on a recording.** Both are on the
  raw events only, and the ledger's needs (`docs/view/README.md`'s table,
  held to the code by `tests/view_model.rs`) ask `cpu.oncpu_time` for
  `timeline,summary` and `io.syscall` for `summary,hist`. Adding `raw` is a
  change to `NEEDS` here *and* to core's table and fixtures, so it is
  requested below rather than made; until then the sheet says on each line
  that the run did not keep them.
- **Running `omn top` on a terminal.** This shell has no `/dev/tty`; the
  sheet was judged on its 120x40 snapshots only.

## Privileged runs wanted

None for this stream. After the fixtures are re-recorded (below), on the
development machine with the capabilities:

```console
make
tests/view_fixtures/record.sh
OMN_VIEW_BLESS=1 cargo test --test view_snapshots ledger
OMN_VIEW_BLESS=1 cargo test -p omn-view --lib ledger
```

## Decision lines for DECISIONS.md

- **The ledger charges energy to a process by cpu share, and says so.** RAPL
  counts a package; the sheet shows the machine's joules as measured and,
  under them, the target's share of the machine's busy cpu time applied to
  them, in italics, labelled "an estimate, not a measurement". Why: the
  catalog requires a per-process attribution to be labelled a derived
  estimate, and an unlabelled split would read as a measurement.
- **Every ledger line carries what knowing it cost.** The collectors'
  attributed cpu plus their in-target cpu, `≥` where the latter is unknown,
  per line and in total against the target's own account. Why: the cost of
  observation is a resource the process paid; on `http-server` the I/O line
  cost over twenty times the time it measured, which nothing else on screen
  says.
- **A line with nothing of the focused task says so, never zero.** Why: a
  per-process source keyed on the process (`fs:proc`) has no series for a
  thread; drawing 0 would claim the thread spent nothing.

## Needs from another stream

- **core** (`docs/view/README.md`'s needs table, `tests/view_fixtures/`):
  add `raw` to the ledger's `cpu.oncpu_time` need (16-32 events a run at
  250 ms) and to its `io.syscall` need (kernel rows: 7 to 364 on the
  fixtures), so the user/system split and the bytes read and written are on
  a recording, then re-record. The change to `NEEDS` in `ledger.rs` is two
  words and lands with the table; the view already reads both from raw
  events.
- **core** (`view/src/snapshot.rs`, `tests/view_snapshots.rs`): a way to
  draw `--against` and a focus in the per-view snapshot test, so views that
  compare or narrow are judged in the gallery's standard set rather than in
  each view's own unit test (the ledger's two extra snapshots are written
  from `ledger.rs`'s tests for now).
- **core** (`view/src/units.rs`): an `approx` (three significant digits for
  a measured float, `208` / `53.6` / `7.19`) -- `units::count` prints
  `208.47` for a non-integer under a thousand. The ledger carries its own
  for joules and watts.
- **coordinator**: the fixtures re-recorded after cupti's merge, so
  `gpu.kernel` is served on one of them and the GPU time line is drawn on
  real data (a GPU workload fixture: `tests/gpu_workload.py`).

## Review

One fix round, after an adversarial review. Every finding was taken; none
was judged wrong.

- **defect -- the `--against` ratio of the I/O line was of another number
  than the one beside it.** The line compares the time inside the calls
  (`value`), but printed the second run's call count next to the ratio
  (`82 calls ×0.96`). `Served` now carries `compared_as`, the figure the
  comparison prints where the amount is another number: the I/O line reads
  `170µs inside ×0.96`. The time stays the compared figure because it is
  what the calls cost. Held by `the_comparison_prints_the_figure_its_ratio_is_of`;
  the snapshot `ledger--against--cpython-hot-heavier` and
  `docs/view/ledger.md` re-said.
- **gap -- network volume was dropped without a word.** A **network** line
  now sits before retransmits: the bytes sent and received through the
  target's sockets, from `net.socket`'s rows (each row's `bytes` already
  summed over its calls, so never weighted again), per task, with the
  number of socket calls and a sparkline of bytes over the run; `enter`
  opens `flows`. The need is `net.socket` `trace` `raw,hist` `target`, the
  same request flows and heatmap make, so the fixtures already hold it and
  one collector serves the three views; its row is added to
  `docs/view/README.md`'s table (the ledger's own row, which
  `tests/view_model.rs` holds to the code). On `http-server`: 28.3 MiB out,
  1.76 KiB in, 176 calls, one thread. Held by `network_volume_is_the_rows_bytes`.
  The sheet's total cost grows by that collector's cpu (on `cpython-hot`
  ≥38.6 ms became ≥72.1 ms: a socket collector that watched no socket).
- **nit -- the focus test checked less than its name.** It now walks every
  per-task line (cpu, memory held, faulted, I/O, network, GPU memory): each
  is no more than the whole or says it holds nothing of the task; faults and
  I/O are strictly less; cpu, memory held and GPU memory are the `--` of a
  process-keyed source; retransmits and energy unchanged.
- **nit -- bytes of an `op` "unknown" were counted as read.** They are now
  said apart ("by calls of no known direction") and added to neither; the
  network line does the same for an op neither `send` nor `recv`. Held by
  `bytes_of_no_known_direction_are_neither_read_nor_written`.
- **nit -- the peak of several series was the sum of their maxima.** It is
  now the highest of their sum at any one time (`peak_of_levels`, held by
  `levels_peak_together_not_each`); with a summary and no series over time,
  the sum of maxima is kept and the detail calls it "at most".
- **nit -- a live run with no task and no account was titled "the
  machine".** `machine_run` now needs the target's account to be known and
  equal to the machine's; with neither known the title says "the target or
  the machine". Held by `a_live_run_without_tasks_does_not_claim_the_machine`.
- **nit -- a narrow column cut the latency distribution.** Adjacent log2
  buckets are now added together until every bucket fits; the bounds stay
  beside the bars while that leaves room, else the bars are drawn alone and
  the detail names the bounds (it now always does). Held by
  `a_narrow_distribution_keeps_its_tail` at 8, 12 and 24 cells.
- **nit -- a `--` line under a focus drew the whole result's cost and
  trust.** It draws neither now, and the sheet's total, under a focus, adds
  that it is the whole run's: a collector's time is not split per task.
  Held by `a_line_without_the_focus_claims_no_cost`; the snapshot
  `ledger--focus--http-server` re-blessed (its title also names the thread,
  `Thread-1 (proce`, from the socket rows' `comm`, truncated by the kernel).

Every ledger snapshot was re-blessed for the network line.

## The last check

**Review round.** `make check` went red in `omn --lib` on three tests outside
the view crate (`a_profile_of_the_machine_finds_this_thread_for_the_cpu_it_burned`,
in the flaky tally; `a_windowed_request_s_collection_states_its_own_window`;
`an_ibs_sample_says_which_level_answered_the_load`), with the load average
at 40-60. The `cargo test --workspace --no-fail-fast` that followed: 2366
passed, 6 failed, every one outside the view crate and in code this diff
does not touch (`page_faults` sampling, `cases`'
`every_case_names_its_known_cause_both_ways`, the capability-refusal test,
`fp_leaf`, a wakeup join refusal, a profile's per-task losses); clippy
`-D warnings` and `fmt --check` passed, and the binary carried
`cap_bpf,cap_perfmon`. Every view test passed: `omn-view` (38),
`view_snapshots`, `view_model`, `view_live`, and `cli`.

**First round.**

`make check` (clippy and fmt passed) went red on one library test,
`collect::cpu::profile::tests::a_profile_of_the_machine_finds_this_thread_for_the_cpu_it_burned`,
which is in the flaky tally (`docs/open-findings.md`); rerun alone, it
passed. The one `cargo test --workspace --no-fail-fast` that followed: 2077
passed, 14 failed, every failure outside the view crate and in a target this
stream does not touch -- `cases` (one cited number 937 ms off its reference),
`family_gpu`, `runner`, `scenario_net` (7: drop reasons, queue windows,
retransmits), `scenario_proc` (1), `triage` (2), and `omn-mcp`'s
`walkthrough` (a `--budget 25%` exceeded at 29.9 %). The machine's load
average was 30-35 while it ran, with the other streams' builds and suites
beside it, and the binary still carried `cap_bpf,cap_perfmon`. The diff of
this branch is `view/src/views/ledger.rs` and documents only; every view
test passed (31 in `omn-view`, `view_snapshots`, `view_model`,
`view_live`). Worth a rerun of those targets on a quiet machine at the
merge, not an investigation from here.
