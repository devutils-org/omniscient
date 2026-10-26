# The nineteenth wave, stream flame

Branch `feat/w19-flame`, from `main` at `91a0cef`. `docs/dev/BRIEF.md`, "flame -- a
flamegraph in the terminal": from `cpu.profile` and `sched.offcpu` stacks, an
icicle that zooms into a frame and back out, search by name, on-cpu and
off-cpu side by side, and the difference of two runs (`omn top --replay <a>
--against <b>`), red where the second is slower and blue where it is faster,
from the triage's own diff. Files touched: `view/src/views/flame.rs`,
`docs/view/flame.md`, `docs/view/gallery/flame*`, and this handoff --
nothing else.

## What landed

- **`view/src/views/flame.rs`** replaces the stub. Its needs are unchanged
  (`cpu.profile` sample@997hz and `sched.offcpu` trace, `stack` with `fp`),
  so `docs/view/README.md`'s table and `tests/view_model.rs` still hold.
  - The view's name and title on a rule above it (`tests/view_live.rs`'s
    gallery test looks for one or the other).
  - Two icicles, on cpu and off cpu, root at the top, heaviest callee
    leftmost, one colour per function name (warm on cpu, cool off), never the
    colour of the left neighbour.
  - **Folded chains**: a frame whose heaviest callee holds it to within one
    cell shares a box with it, so each column is as deep as it branches. An
    88-frame kernel path under CPython fits 40 rows.
  - Under each icicle, the heaviest functions on their own (with
    `trust::error_bar` for a lossy run) and then the source's caveats, each
    whole or not at all, with a `‼ N more` line when some do not fit; the
    bottom line carries the first left over, and `!` lists every caveat of
    both panes and both runs whole. Above, each pane's heaviest own
    function; below, the selected box whole.
  - Keys: `↑↓`/`jk` caller and callee, `hl`/`[]` along a row, `⏎` zoom,
    `⌫`/`u` out to the nearest caller that shows more, `/` and `*` search
    (share of the profile matched, a match under a match counted once), `o`
    switch pane, `m` one pane alone, `t` off cpu by thread, `d` absolute or
    shape comparison, `!` every caveat whole, `esc` reset.
  - The frame's focus narrows off cpu from the raw blocks (`tid`, `pid`);
    on cpu says it cannot ("these stacks carry no thread").
  - Live: no stack names until the end, so a pane says what arrived on how
    many stacks, with a sparkline of arrivals, dimmed `live`.
  - **`--against`**: a merged tree of both runs, width = mean of the two,
    colour by the triage's rule (`src/triage/diff.rs`, restated: absolute,
    one rate in samples and two in time, 3σ of Poisson noise with variance
    count x scale², both runs' losses said). `d` scales the second run to
    the first's total. Off cpu, which has no noise model, uses
    `trust::difference_colour` and ignores specks under 0.25 %.
  - 21 unit tests, among them the triage's four cases with their inputs
    verbatim (one function doubled, shares, noise, two rates in time) and
    "a run against itself has none", every sample in
    the tree once on four fixtures, the thread split and the narrowing
    against the raw events, the live pane against `as_live_until`, and
    the key path (zoom, out, search).
- **Gallery**: the harness's seven `flame--<fixture>` snapshots, and nine of
  the view's own drawn by its unit tests -- `flame-against--…`,
  `flame-against-shape--…` (cpython-hot vs cpython-hot-heavier),
  `flame-zoomed--`, `flame-search--`, `flame-threads--`, `flame-narrow--`
  (80x24) on http-server, `flame-live--sqlite-commits`.
- **`docs/view/flame.md`**: the screen, the keys, the comparison's
  arithmetic, what each snapshot shows, what the view does not do, and the
  verdict.

## Shown versus reasoned

Shown, by a test or a snapshot drawn from the committed recordings:

- Every sample lands in the tree once, and no frame is narrower than its
  callees (cpython-hot, http-server, sqlite-commits, sqlite-latency).
- sqlite-commits' off-cpu time is `fdatasync`'s, all of it.
- cpython-hot against cpython-hot-heavier: **absolutely, 38 of 378 functions
  moved beyond 3σ and every one up** (x2.03 overall; `__tls_get_addr`
  +408, 12.3σ); **in shape, none**. Asserted as a separation (absolute at
  least four times the shape's count), not a threshold.
- http-server off cpu splits into three threads and narrows to one exactly
  (the sum of its raw blocks' `total_ns`).
- Every caveat under an icicle is said whole or counted, at every height
  from 2 to 40 rows (`caveats_are_said_whole_or_counted`); `!` lists every
  one whole; a comparison says the second run's caveats too.
- Off-cpu blocks with no stack weigh the same through the stacks and
  through the blocks, and a run that kept no blocks says how many are in no
  box (a synthetic run: every fixture has `unattributed` 0).

Run by hand, not by a test: `omn top --replay … --against … --headless
--view flame` draws the comparison through the real subcommand
(`tests/view_live.rs` only checks that `--against` without `--replay` is
refused).

Reasoned, not shown:

- **Live on cpu.** The fixtures keep `cpu.profile` as `stack` only, so
  `as_live_until` gives its pane no events. `flame-live--sqlite-commits`
  now says so on screen (a degradation the snapshot's test adds to the
  replay, first under the count) instead of passing zero samples off as a
  run. A real live run hands over the raw samples (they carry `stack_id`),
  so the pane should count them; no test runs a live `omn top` with this
  view on a terminal, and this agent shell has none.
- **On a terminal.** The snapshots' `.ansi` carry the colours; nobody has
  looked at the view in a real terminal yet, and the keys were driven
  through `App::key` only.
- The restated diff agrees with `src/triage/diff.rs` by construction and on
  the triage's four cases' inputs and expected answers, not by a test that
  runs both codes on one input (the view links nothing of `omn`; see the
  requests).

## What I could not do, and why

- **Python functions.** `hot.py` is one Python function's time; the icicle
  shows the interpreter's unnamed C frames. The triage charges a sample to
  the innermost frame a perf map named; the view has no perf map to read --
  `results.json` cites the maps in a note but does not carry their names in
  the mirror. Needs a fixture recorded with them, and the charge restated.
- **Search for names holding `q`, `?`, a digit, or `f` under a focus**: the
  frame takes those keys first (`app.rs`). `sqlite3+0x…` cannot be typed;
  `*` on a selected box works around it.
- `fp` stacks lose every frameless leaf's caller: half of cpython-hot's
  samples. Said under the icicle; not repaired.

## The check

One `make check` before the last commit: red in `cases`
(`every_case_names_its_known_cause_both_ways`, `every-cited-number-agrees-
with-the-reference` on gcc-shell and sqlite3-stripped: on-cpu against the
program's own clock, beyond the resolution -- in the flaky tally,
`docs/open-findings.md`), with three other suites running on the machine at
once. One `cargo test --workspace --no-fail-fast` then: 773+1509 passed in
the green binaries, and eleven red tests, none in a file this stream touches
except `a_gallery_prints_every_view`, which was this view (it had no title
the test could find; fixed, `tests/view_live.rs` green). Each of the other
ten rerun alone, once: eight passed; `scenario_mem`'s
`the_two_roads_to_a_footprint_arrive_at_the_same_number` passed on a second
alone run; `runner`'s
`a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed`
failed again -- the kernel throttled the profile 4 335 times under the load,
and its own message says throttles are not placed per ring ("filed for stream
profile"). Clippy and `cargo fmt --check` green. The scratchpad's `nff.log`
is shared by every stream's agent and was overwritten during the run; the
counts above are from this worktree's own output.

## Privileged runs wanted

None. Nothing here touches a collector or the machine.

## Decision lines for docs/DECISIONS.md

- **The flame view folds a call chain that never visibly branches into one
  box** -- a frame whose heaviest callee holds it to within one cell at the
  current zoom. An icicle's depth is then where it branches; the detail line
  says what a box folds.
- **A comparison is drawn on the mean of the two runs**, linear so a caller
  is never narrower than its callees, and judged by the triage's arithmetic
  restated in the view (absolute, 3σ Poisson, losses said); `d` compares
  shapes, the second run scaled to the first's total, because "the same
  program doing more" and "a different program" are different answers and
  each mode gives only one.
- **Off-cpu time has no noise model**: a comparison colours it by
  `trust::difference_colour` (±5 %) above a 0.25 % speck, and says so
  instead of a σ.
- **No icicle while a run is live**: stacks are named at the end, so a live
  pane counts arrivals and draws when they came.

## Needs from another stream

- **core (`view/src/app.rs`, `views.rs`)**: a way for a view to take text --
  e.g. `View::takes_text(&self) -> bool`, consulted before the frame's own
  keys -- so a search can contain `q`, `?`, digits and `f`. Today `/sqlite3`
  jumps to view 3.
- **core (`tests/view_*.rs`)**: a test holding the view's restated diff to
  `src/triage/diff.rs` on the two cpython fixtures (same `difference` list in
  both modes the triage has), since only the root crate's tests can link
  both. Or move the arithmetic into a crate both depend on.
- **core (`tests/view_fixtures/record.sh`)**: a CPython fixture recorded with
  the interpreter's perf map (`PYTHONPERFSUPPORT=1`) and `--stack fp+leaf`,
  and the perf map's names in the mirror, so the flame view can charge to
  Python functions as the triage does.
- **core (`view/src/feed.rs`)**: `as_live_until` could add the degradation
  the flame test adds by hand -- a source the recording kept without `raw`
  arrives empty when replayed as live -- so every view's live snapshot says
  it, not only this one's.
- **core (`view/src/app.rs`)**: at 100 columns the header runs the span into
  the cost corner (`3.89sview: snapshot`), seen with `--size 100x30`.

## Review

The adversarial review's six findings, one fix round:

1. **Caveats cut off unsaid (defect) -- fixed.** `ranking()` now wraps each
   caveat itself (`wrap_words`, nothing dropped) and says it whole or not at
   all, after the ranking's first three entries, then `‼ N more` when some
   do not fit; it returns how many it said, and `bottom()` carries the rest
   (`(+N more, ! lists them)`). `!` lists every caveat of both panes, whole.
   Nothing under the icicle wraps in ratatui any more, so nothing is clipped
   at the bottom. `flame--cpython-hot` now shows the `stacks unwound by fp`
   and `names came from` caveats under the icicle; `flame--http-server`'s
   on-cpu pane says three whole and "6 more", and the bottom line the next.
2. **Off-cpu blocks with no stack dropped (defect) -- fixed.** Through the
   stack emit, their time is read from the blocks the run kept (`raw`) and
   drawn under `[no stack]`, so the total matches the block path; without
   the blocks the pane's aside says how many are in no box. On the block
   path a block with no `total_ns` is counted and said, not summed as 1 ns
   (the reviewer's last nit).
3. **Only the first run's caveats (gap) -- fixed.** The second run's are
   said after the first's, prefixed `the other run:`; its asides likewise.
   `compare()` takes every result of each side and sums their losses.
4. **The live snapshot's zero (gap) -- fixed on screen.** No fixture keeps
   `cpu.profile` raw, and recording one is core's (`record.sh`) and needs a
   privileged run, so the snapshot's test says it: a degradation on the
   replayed result, drawn first under the count. A live pane now draws its
   source's caveats under the count. Request filed for `as_live_until`.
5. **Handoff over-claims (nit) -- fixed.** The `--against` subcommand run is
   listed as a manual run; the triage's four cases (two rates included) are
   now restated with their inputs verbatim.
6. **Units and the view's own rule (nit) -- fixed.** See 2 for the weight
   fallback; the off-cpu sentence says the ±5 % and 0.25 % rule is the
   view's, not the triage's; in shape mode the line above prints the raw
   totals beside their raw ratio (`x2.03, scaled to one`), not the scaled
   ratio of 1.00.

### The review round's check

`cargo test -p omn-view --lib views::flame`: 21 passed; `view_snapshots` 15
passed; clippy on `omn-view` and `cargo fmt` clean. One `make check`: red at
the `omn` lib on `a_profile_of_the_machine_finds_this_thread_for_the_cpu_it_burned`
(264 samples for 199 expected; in the flaky tally), green alone. Then one
`cargo test --workspace --no-fail-fast`, with the machine at a load average
of 66 (other streams' suites): 2380 passed, 7 red, none in a file this
stream touches. Each rerun alone, once: the two `collect::power::energy`
tests, the three in `budget`, and `cases`'
`every_case_names_its_known_cause_both_ways` passed; `runner`'s
`a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed`
failed again with the same message as the first round -- the kernel
throttled the profile 4 116 times and the throttles are not placed per ring
(already filed for stream profile).
