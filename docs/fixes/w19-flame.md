# The nineteenth wave, stream flame

Branch `feat/w19-flame`, from `main` at `91a0cef`. `BRIEF.md`, "flame -- a
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
    `trust::error_bar` for a lossy run) and then the source's caveats in
    full; above, each pane's heaviest own function; below, the selected box
    whole.
  - Keys: `↑↓`/`jk` caller and callee, `hl`/`[]` along a row, `⏎` zoom,
    `⌫`/`u` out to the nearest caller that shows more, `/` and `*` search
    (share of the profile matched, a match under a match counted once), `o`
    switch pane, `m` one pane alone, `t` off cpu by thread, `d` absolute or
    shape comparison, `esc` reset.
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
  - 16 unit tests, among them the triage's own "one function doubled is the
    only difference" and "a run against itself has none", every sample in
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
- `omn top --replay … --against … --headless --view flame` draws the
  comparison through the real subcommand.

Reasoned, not shown:

- **Live on cpu.** The fixtures keep `cpu.profile` as `stack` only, so
  `as_live_until` gives its pane no events, and `flame-live--sqlite-commits`
  shows an on-cpu pane with zero samples. A real live run hands over the
  raw samples (they carry `stack_id`), so the pane should count them; no
  test runs a live `omn top` with this view on a terminal, and this agent
  shell has none.
- **On a terminal.** The snapshots' `.ansi` carry the colours; nobody has
  looked at the view in a real terminal yet, and the keys were driven
  through `App::key` only.
- The restated diff agrees with `src/triage/diff.rs` by construction and on
  the triage's own test case, not by a test that runs both on one input
  (the view links nothing of `omn`; see the requests).

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

## Decision lines for DECISIONS.md

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
- **core (`view/src/app.rs`)**: at 100 columns the header runs the span into
  the cost corner (`3.89sview: snapshot`), seen with `--size 100x30`.
