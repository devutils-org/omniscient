# The twentieth wave, stream view

Item 2 of `STATE.md`'s "What remains", the frame's half: what the nine views
asked of the frame and of `results.json`, the links between views, the mouse,
and the `process` view registered as a stub. Branch `feat/w20-view`, three
commits before this one (`d7cebb7`, `e548ce2`, `0276a65`).

## What landed

**The links between views** (`view/src/app.rs`, `views.rs`).

- The frame keeps a history of every `Action::Goto` (the view and the focus
  it left). `⌫` goes back one while there is a history, and a right click
  does the same; a `Goto` to where the frame already is adds nothing.
- The links that already existed (system → ledger, ledger line → its view,
  timebar → request or lanes, heatmap → request or lanes, request → lanes by
  `l`, wake → lanes by `l`) now come back. `request`: a second `enter` on a
  focused request opens its threads in `lanes`, and `w` opens `wake` on it.
  `wake`: a second `enter` on a focused thread opens its `lanes`.
- The flame's own `⌫` (zoom out) is shadowed while there is a history; its
  `u` does the same, and is said in the README.

**`process`, registered as a stub** (`view/src/views/process.rs`): the tenth
tab, reached by `0`; no needs of its own (it reads what the others ask, so it
never changes what a live run collects); draws `draw_stub` until stream
process fills it. **The first screen** is chosen in `App::new`: `process`
when the run has a target of its own (`Data::has_target`: a result carries a
`lifetime`), `system` otherwise. For a live run with `--pid` or a launched
program nothing in the opening data says so yet: filed to budget below.

**The mouse** (`views.rs`, `app.rs`, `term.rs`, every view).

- `View::regions()` -- what the view drew that a click can select, each a
  `Region { rect, id }` rebuilt at every draw -- and `View::select(id)`,
  which moves the selection exactly as the keys would. `View::wheel(down)`
  defaults to `↑`/`↓`; the heatmap overrides it to zoom in time.
- The frame: a left click on a tab opens its view; on a region it selects;
  on the selected region again it goes in (`select` then `enter`); a right
  click goes back; the wheel goes to the view. `M` toggles capture (on by
  default); the help says what the mouse does and that `M` gives the
  terminal its text selection back.
- `term.rs` enables crossterm's mouse capture (SGR, `?1006`), keeps it in
  step with `App::mouse`, ignores motion, drag and release (no redraw for
  them), and disables capture on every way out, the panic hook included.
- Regions per view: system's process rows, ledger's lines (not those the
  detail pane covers), request's list rows (the scrolled window), timebar's
  bars, heatmap's map columns (a click puts the cursor on the column's
  middle), lanes' lanes (one or two rows each), wake's grid cells (the page
  holding the root), flows' objects (both bands), flame's boxes in either
  pane (a click also makes that pane the active one).
- The snapshot harness: `snapshot::click`, `right_click`, `wheel`, and
  `find(buffer, text)` to click on what a test reads.

**The frame** (the rest of item 2's list).

- `View::takes_text()`: while it is true every key is the view's, `q`, `?`,
  `f`, digits and `⌫` included; the frame keeps `Ctrl-C`. The flame returns
  it while a search is typed.
- `snapshot::draw_with(data, view, against, focus, w, h)`: `--against` and
  a focus in the harness.
- `units::approx`: three significant digits for a measured float (`208`,
  `53.6`, `7.19`, `0.0412`), a count from a thousand up.
- `feed::as_live_until` says, in the result's degradations, that a source the
  recording kept without `raw` arrives empty; the flame's test no longer adds
  it by hand.
- A test in `tests/view_model.rs` holds `views::flame::difference` (new, a
  public reading of the on-cpu comparison) to `omn::triage::diff::diff` on
  `cpython-hot` and `cpython-hot-heavier`, both directions: same unit, same
  functions, same order, and not empty.
- The README test now checks each needs row whole: emits, keys, stack,
  scope and the `why` column.
- `frame--gallery` and `frame--live` are drawn on `ledger` instead of
  `heatmap` (in `tests/view_snapshots.rs`, record's file: see below).
- A scale-aware dimming rule: `Trust::dims_at(cell_ns)` and `style_at` -- a
  time error dims only when it is a tenth of a cell or more
  (`TIME_ERROR_CELLS`); every other mark dims as before, and the badge stays.
  Used by the lanes' cells.

**`results.json`** (`src/emit.rs`, `src/encode.rs`).

- `Output::Timeline` carries `first_ts_ns`, the moment every `offset_ns` is
  from; the system view's `timeline_origin` uses it when present and keeps
  its inference for older recordings.
- `Results` carries `value_attr`; the view's model mirrors it and the
  heatmap reads a source's value by it, keeping its table for older
  recordings.

**Documents**: `docs/view/README.md` (keys, the first screen, the contract's
new methods, "The links between views", "The mouse" with what a terminal and
tmux need), `man/omn.1` (`omn top` in the synopsis and the commands, a new
section THE VIEW), `GUIDE.md` (a section "`omn top`: the views").

## Shown versus reasoned

Shown, by tests in this branch:

- `app::tests` (10): the first screen on a fixture with and without a
  target; `enter` system → ledger → flame and `⌫` twice back to system
  unfocused with an empty history; `0`; a flame search typing `q?3f` without
  quitting, opening the help or jumping, then `3` jumping once the search is
  done, and `Ctrl-C` quitting; a tab click; a click that selects, a second
  that goes in and lands on the same focus as `↓↓enter`, a right click back;
  the mouse off ignoring clicks and the wheel; every region of every view on
  every fixture inside the body; every view with a selection reporting
  regions on a fixture that has one, and twenty clicks on each answered
  without a panic.
- `trust::tests::a_time_error_dims_only_at_a_scale_it_can_move`,
  `units::tests::approximations_keep_three_digits`,
  `feed::tests::a_source_kept_without_raw_says_it_arrives_empty`.
- `tests/view_model.rs`: the flame/triage test and the stricter README test.
- `cargo test -p omn-view` 173 passed; `view_model` 5, `view_live` 5,
  `view_snapshots` 16, `view_timebar` 1; `--lib emit encode
  triage::latency` 119; `encoders` 15; clippy and fmt clean.

Reasoned, not shown:

- **The mouse in a real terminal and through tmux.** No terminal here: the
  harness drives `App::mouse` with crossterm's own `MouseEvent`, so the
  routing is tested, and crossterm decodes SGR; that a Ghostty + tmux `mouse
  on` session delivers those events is crossterm's and tmux's documented
  behaviour, not observed. It is the first thing for the user to try.
- **That each view's regions match its drawing cell for cell.** Each region
  is computed from the same layout the view draws with (and the test holds
  them inside the body), but only system's was checked against the keys'
  result; for the others a click selects what is drawn there by reading the
  code, not by a test per view. Per-view click snapshots are for record.
- **`first_ts_ns` and `value_attr` on real recordings.** The fixtures predate
  them, so both read paths are exercised only on the fallback; the
  re-recording in phase B carries them.

## Every snapshot redrawn

Every `docs/view/gallery/*.{txt,ansi}` the harness and the views' own tests
draw changed, blessed on the current fixtures: the tabs line (the tenth tab),
the footer (unchanged keys, but `⌫ back to …` appears when there is a
history), `frame--help` and `frame--narrow` now on `process` (the http-server
fixture has a target), `frame--gallery` and `frame--live` on `ledger`, the
flame's live snapshot with `as_live_until`'s sentence instead of its own. I
read the diff of every `.txt`: nothing else moved. The new
`process--<fixture>` snapshots are the stub's; stream process re-blesses
them.

## What I could not do, and why

- **Clicks per view in the gallery** (a snapshot of each view after a click):
  `tests/view_snapshots.rs` is record's; the harness has what it needs.
- **`--against` and a focus in the standard snapshot set**: same file; the
  ledger's two extra snapshots stay in `ledger.rs` until record moves them to
  `draw_with`.
- **A ledger line that opens `timebar`**: the sheet has no line of blocked
  time, so the "Decided" link "a ledger line opens … `timebar` (blocked
  time)" has nothing to start from. Adding the line is the ledger's design
  (the views were kept as drawn), so I left `ITEMS` alone and said it in the
  README's table.
- **`STATE.md`'s tables** (item 2 says "and this file's tables"): the
  coordinator's.

## Files outside my row, changed

Each is a mechanical consequence of a change to a shared layer I own, and
the merge should keep it:

- `src/triage/latency.rs` (triage) and `tests/encoders.rs` (nobody's): one
  `value_attr: String::new(),` in each `Results` literal, since the struct
  gained the field.
- `tests/view_snapshots.rs` (record): `process` added to the `views!` list
  (otherwise `every_registered_view_has_its_test` fails once the stub is
  registered), and the two frame snapshots moved from `heatmap` to `ledger`
  (item 2's "drawn on a view that changes less").
- `docs/view/gallery/**` (record): the re-blessed snapshots above.
- `man/omn.1` (shared): the `omn top` lines the brief gave me "after
  shared's"; they are new blocks, so they should merge beside shared's.

## Privileged runs wanted

None for this stream: nothing here touches a collector. One run for the user
at a terminal (not privilege):

```console
cd /path/to/omniscient && make
target/debug/omn top --replay tests/view_fixtures/http-server
#   click a tab, a process row in `system` twice, right click, the wheel on
#   the heatmap; M, then select text with the mouse; repeat inside tmux with
#   and without `set -g mouse on`
target/debug/omn top -- python3 -c 'import time; [sum(range(10**6)) or time.sleep(0.01) for _ in range(300)]'
```

## Decision lines for DECISIONS.md

- **A click selects, a second click on the selection goes in; there is no
  double-click timer.** crossterm reports no double click, and a timer would
  make the snapshot harness depend on time. Cost of leaving it: none known.
- **The tenth view is reached by `0`, and its tab is labelled `0`.** The
  existing `1`-`9` keep their views, so no habit and no test changed.
- **`⌫` is the frame's while there is a history**, a view's otherwise (the
  flame's zoom out, also on `u`).
- **The process view asks for nothing of its own**: its tiles read what the
  other nine ask, so adding it changed no live session.
- **A time error dims a figure only at a resolution it can move** (a tenth
  of a cell), drawn at a known resolution; elsewhere it dims as before.

- **A collector that does not say how it stamps its rows says nothing**
  (`row_stamp` absent), rather than defaulting to "at its own event": a
  reader falls back to what it knows, as the heatmap's table does.
- **Drops are placed at the drains of a drained collector only**; a polled
  collector's are read once, at its stop, since a read of its losses each
  round would cost a clone for a count that is almost always zero.

## Needs from another stream

- **budget (`src/main.rs`, `top_app` / `top_live`)**: open `process` as the
  first screen of a live `--pid` or launched-program run when no `--view` is
  given (`app.open("process")`), since a live run's opening data has no
  `lifetime` to read it from. And, when the live results are built
  (`omn_view::model::Results { .. }` near `top_live`), set `value_attr` from
  the resolved collection's value attribute so the heatmap reads it live too.
- **budget (`src/main.rs`, `top_live`)**, beside `value_attr` above: set
  `row_stamp` and `drains` on the live `omn_view::model::Results` from the
  collection, so a live heatmap reads them too.
- **heatmap's owner, next wave**: shade a drop in the column of the drain
  that read it (`Results::drains`), in the loss strip, instead of across the
  whole window; no fixture holds `drains` until record re-records.
- **record (`tests/view_snapshots.rs`, `docs/view/gallery/**`)**: keep the
  two changes above; draw each view's `--against` and focus snapshots with
  `snapshot::draw_with` (and move the ledger's two from `ledger.rs`); add a
  snapshot per view after a click (`snapshot::click` on `snapshot::find`).
- **process**: the stub is `view/src/views/process.rs`; `regions`/`select`
  for the tiles, and `Action::Goto` for each tile's view, are what the frame
  routes; `Data::has_target` is the first-screen rule.
- **ledger's owner, next wave**: a line for blocked time, so the ledger opens
  `timebar` as "Decided" asks.

## Review

The reviewer found item 2's last two `results.json` entries not landed: how
a drained source stamps its rows, and each drain's drop count with its time.
Right: the first round filed them to the collectors' streams, but both are a
field the runner can fill and a method each collector can answer, so this
round lands them (one commit after the merge of `main`).

- **`Collector::row_stamp()`** (`src/collect/mod.rs`), `None` by default --
  which says nothing, and is not read as "each row at its own event" -- and
  `RowStamp::{DrainClose, FirstEvent}`. Answered by every collector whose
  rows I read stamped from a table the kernel fills between drains:
  `FirstEvent` for `sched.runqlat`, `sched.futex`, `sched.irq`, `sched.lock`,
  `net.softirq`, `net.qdisc` and `mem.stall` (traced; counted it is
  `DrainClose`); `DrainClose` for `io::histogram` (`net.socket`, `io.vfs`),
  `sched.offcpu` and `mem.allocations`. Every other collector says nothing
  (its owner can add the line). Carried through `Collection.row_stamp`,
  the detach record and a recording's documents into `Results.row_stamp`
  (`"drain_close"` / `"first_event"`, absent when unsaid).
- **`Collection.drains` / `Results.drains`**: `[{ts_ns, dropped}]`, one
  entry per read of a drained collector (one with a `tick`) whose drop count
  had grown, with that read's time; the detach's and the run's final stop
  add what the stop read. The count is the collection's running total (the
  runner's losses plus the collector's), so a budget's move onto another
  backend neither loses nor double-counts; the entries sum to
  `losses.dropped`. A polled collector's losses are read only at its stop,
  for what a clone of its `Losses` every round would cost. A recording's
  document holds the drains read inside its window.
- **The view**: `model::Results` mirrors both (`row_stamp: Option<String>`,
  `drains: Vec<Drain>`); the heatmap's `Stamp::of` takes the result's own
  stamp and keeps its table for older recordings. The loss strip does not
  draw `drains` yet (filed above). `docs/view/heatmap.md`'s limits say so.

Shown: `collect::tests::a_drained_collector_s_drops_are_placed_at_the_drains_that_read_them`
(a drained and a polled collector under `drive`, a detach at 60 ms: the
entries are in order, each non-zero, sum to the drop count, the last is the
stop's five at the detach; the polled one has none until its stop);
`emit::tests::a_result_carries_its_row_stamp_and_its_drains` (through
`aggregate` and into JSON, and absent when unsaid);
`heatmap::tests::a_result_s_own_row_stamp_wins_over_the_table`;
`tests/view_model.rs`'s
`the_view_reads_a_result_s_row_stamp_and_drains_as_omn_writes_them`.
Reasoned: that each named collector stamps as it says, read from its code
(the `first_ns`-or-close and `closed_ns` stamps), not measured on a run;
and the real drop placement on a privileged run (none here drops).

Files outside my row, changed this round: `src/collect/mod.rs` (the
runner's ledger, `Collection`, the trait), the ten collectors' one-method
`row_stamp` (sched, net, mem, io families), `src/encode.rs`,
`src/triage/latency.rs` and `tests/encoders.rs` (two lines per `Results`
literal), `docs/view/heatmap.md`.

## The suite

One `make check` before the last commit (the shared layers `emit.rs` and
`encode.rs`): it stopped at the library's tests on one failure, so the whole
suite was then run once with `cargo test --workspace --no-fail-fast`, on a
machine nine agents share. Every view, emit and encode test passed. Seven
failures, none in a file this stream changed; each rerun alone once:

| Test | Owner | Alone |
|---|---|---|
| `collect::cpu::profile::tests::a_profile_of_the_machine_finds_this_thread_for_the_cpu_it_burned` (276 samples for ~199 expected) | rings | passed |
| `perf::tests::an_ibs_sample_says_which_level_answered_the_load` | rings | passed |
| `tests/triage.rs` `a_program_cpu_bound_then_lock_bound_escalates_to_the_lock_layer_and_its_waits_match_its_own_clock` | triage | passed |
| `tests/runner.rs` `a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed` (3 599 throttles, none in `losses_by_ring`) | rings (item 4, the failure the test itself names) | **failed again** |
| `tests/cross_machine.rs` `a_memory_limit_turns_a_cached_read_into_device_reads_and_each_byte_carries_its_rung` (72 of 39 281 pages outside the program's intervals) | shared | **failed again** |
| `tests/cases.rs` `every_case_names_its_known_cause_both_ways` (`gcc-shell/the-passes-o2-adds`: on-cpu 6 266 ms against `ftime` 7 610 ms, beyond the 256 ms allowed) | docs | **failed again** |

The three that failed twice are filed here for their owners; the runner's is
item 4's own, and the other two measure the machine's load, which nine agents
made heavy.

### The suite, review round

One `make check` after `make` (capabilities granted): it stopped at
`tests/cases.rs`'s `every_case_names_its_known_cause_both_ways` (the mcp
path, docs' item and in the tally), so the whole suite was then run once
with `cargo test --workspace --no-fail-fast`: 2 628 passed, 9 failed, 13
ignored. Each failure rerun alone once:

| Test | Owner | Alone |
|---|---|---|
| `collect::mem::page_faults::tests::sampled_in_user_mode_the_weighed_samples_are_the_faults_the_thread_took` | mem | passed |
| `perf::tests::an_ibs_sample_says_which_level_answered_the_load` | rings | passed |
| `tests/dm_delay.rs` `a_device_delayed_from_outside_is_measured_with_the_delay_above_the_device` | io | passed |
| `tests/triage.rs` `a_cpu_bound_program_is_named_cpu_bound_for_latency` | triage | passed |
| `tests/view_workloads.rs` `lanes_schedule_is_the_manifests` | workloads | passed |
| `tests/cross_compute.rs` `a_lossy_profile_marks_its_row_and_leaves_the_traced_rows_alone` (the row's drop count 6 616 against 16 686 expected, 328 throttles) | rings | **failed again** |
| `tests/scenario_sched.rs` `a_workload_that_is_half_blocked_measures_as_half_blocked` (31 % blocked) | sched | **failed again** |
| `tests/view_workloads.rs` `system_panels_carry_at_least_the_manifests_cost` (2.069 s on cpu for a 1 s program and its child) | workloads | **failed again** |
| `tests/cases.rs` `every_case_names_its_known_cause_both_ways` | docs | not rerun (in the tally) |

None is in code this round changed in behaviour: the runner only adds
`drains` beside the losses it already counted, and `row_stamp` is a new
field nobody else reads. The cross-compute one reads the per-ring split of a
throttled profile, the rings stream's item 4 (the same family of failure
as the first round's `losses_by_ring`); the other two measure cpu and
blocked time on a machine other agents share.
