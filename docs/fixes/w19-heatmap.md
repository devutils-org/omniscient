# The nineteenth wave, stream heatmap

Branch `feat/w19-heatmap`. The `heatmap` view of `omn top`: latency over
time, time across, log2 latency up, density as glyph and colour, for
`io.latency`, `sched.runqlat`, `net.socket` and `gpu.kernel`. Its guide and
verdict are `docs/view/heatmap.md`; its snapshots are
`docs/view/gallery/heatmap--*`.

## What landed

- `view/src/views/heatmap.rs`, the stub replaced. `NEEDS` unchanged, so
  `docs/view/README.md`'s table and the fixtures stand as core left them.
  - **The map**: one column per slice of the run, one row per fraction of a
    log2 octave (every screen row in use, labelled at powers of ten), a row of
    its own for zero; six density steps, logarithmic against the fullest cell.
  - **Around it**: the figure line (count, range, p50, p99, trust badges),
    the regimes (modes apart by an empty octave, their share and when: once,
    all through, between two moments, in bursts at a measured pace or in a
    counted number of bursts), the count per column, the whole run's
    histogram beside the map, the loss strip, the cursor's column (count, p50,
    p99, max) and its slowest measurement in words (program, thread, kernel
    or operation, `rwbs`, size, device, request), the legend.
  - **Reading**: `raw` events only, so live and finished draw alike. A
    kernel-bucketed row is placed at `total_ns / weight` (inside its bucket,
    exact for one wait) and its weight spread over its drain window, found
    from how its collector stamps it (`net.socket` at the close,
    `sched.runqlat` at its first wait) and cut to the run; a whole-run
    kernel histogram row (`weight`, no `window_ns`) and a `total_of` row are
    left out and said; a result without `raw` is said with
    its summary or histogram, never drawn on a time it does not have; a
    refused source is drawn as its refusal; an absent one says how to ask.
  - **Trust**: every cell, bar and figure in `Trust::of`'s style. Losses
    placed where the run can place them (a live run's eviction from the start
    to the first event kept; every other loss across the whole window, "when
    unknown"); late rounds said, not shaded.
  - **Keys**: `s`/`S` source, `h`/`l` (`H`/`L` by eight) cursor, `+`/`-`
    zoom x2 up to x64 around the cursor, `t` narrows every view to the slowest
    measurement under the cursor (pid, tid, request), `⏎` opens its request's
    waterfall when it carries one and its thread's lane otherwise.
  - Sixteen unit tests: the default source, the sqlite regimes apart, a
    kernel row counted as its waits, a close-stamped row over the window
    before its stamp, a first-wait-stamped row ending at its drain, a
    whole-run histogram said and not drawn, a lone spike kept among five
    regimes, regime shares within a zoomed window, the loss strip's words at
    no margin, focus narrowing, `t` and `⏎`, `⏎` onto a request, the scale's
    bounds, every fixture and source at four sizes drawing only inside its
    area, a live run half way, and the extra snapshots.
- Snapshots: the nine `heatmap--<fixture>` of `tests/view_snapshots.rs`,
  and four of the view's own tests: `heatmap--http-server--sched.runqlat`,
  `heatmap--http-server--gpu.kernel` (the refusal),
  `heatmap--sqlite-commits--zoom-x8`, `heatmap--sqlite-commits--live-half-way`.
  `frame--gallery` and `frame--live` are core's snapshots but draw this
  view, so they were re-blessed with it.
- `docs/view/drawings/heatmap--gpu-phases.txt`: a hand-kept drawing, not
  held by a test (moved out of the gallery in the review round, so it is not
  taken for a snapshot), of a run recorded in the scratch directory (14 MB, not committed):
  four programs of `tests/gpu_workload.py` in a row.
- `docs/view/heatmap.md`: the guide, what each snapshot shows, the limits,
  the verdict.

## Shown versus reasoned

Shown, on this machine:

- The sqlite-commits fixture separates into three regimes, each pulsing at
  ~120 ms -- one pulse per commit: ~22 µs synchronous writes (76 %), ~480 µs
  cache flushes (21 %, `WFSM`, 164 on each of two partitions) and ~4.6 ms
  synchronous writes (2.8 %, `WS`). The operations were read off the events
  by hand to check the doc's words, not computed by the view; the regimes,
  shares and pace are the view's own (snapshot, and
  `the_sync_flushes_are_a_regime_apart_from_the_writes`).
- Zoomed x8 on the same run, every commit is a pair of 6.7 ms columns holding
  all three bands (snapshot).
- On vertical-requests the slowest block request is a002's 4.70 ms
  readahead and `⏎` opens request a002, narrowed: the narrowed samples are
  all a002's (`enter_on_a_request_s_measurement_opens_the_request`). The
  4.5 ms band otherwise is 32 synchronous writes no request owns (read off the
  events).
- A kernel row's weight is placed whole: the map adds up to the waits, not
  the rows, and each row's value is inside the row it is drawn in
  (`a_kernel_row_counts_as_the_waits_it_stands_for`).
- `gpu.kernel` through the CUPTI collector on a phased run: 6 243
  operations, the exact number the four programs issued (6 000 `tiny`, 200
  `spin`, 40 `copy`, 3 copies and memsets); the map shows the launch-bound
  phases at ~900 ns, the `spin` band at ~1.8 ms between them, the copies at
  ~250 µs, the cursor on `[memcpy DtoH] 64.0MiB` at 6.20 ms, the clock
  conversion's `±318ns` on the figure line. One run, on a GPU another stream
  was using (`nvidia-smi --query-compute-apps` showed the bench stream's
  process); nothing in it is a timing claim.
- No draw at any size from 30x8 to 200x60, on every fixture, every source,
  zoomed and moved, writes a cell outside the area it is given: the view is
  drawn into a frame larger on every side, pre-filled with a mark, and every
  cell outside the area must still hold it (`every_size_draws_inside_its_area`).
- A drained row is drawn over the window it was counted in: every
  `net.socket` row of http-server within `[ts - window_ns, ts]`, the 807 µs
  call over 163.5-218.5 ms (`a_row_stamped_at_its_close_is_drawn_over_the_window_before`);
  every `sched.runqlat` row of three fixtures from its stamp to at most a
  window later and never past the run
  (`a_row_stamped_at_its_first_wait_ends_at_its_drain`).

Reasoned, not shown:

- That the http-server's two slow socket calls between 164 and 320 ms are the
  first listings of a cold directory: the map shows when they were, nothing
  says why.
- Losses placed in time: no fixture has a loss (every one is clean but for
  late rounds), so the strip's "when unknown" shading and the eviction
  shading are drawn by code no snapshot exercises with a real loss. The live
  half-way snapshot evicts nothing (it holds far fewer than 200 000 events).

## What I could not do, and why

- **Place a dropped record in time.** A collector says how many records it
  dropped, never when; only a live run's eviction can be placed. Filed below.
- **A GPU snapshot held by a test.** The fixtures are core's
  (`tests/view_fixtures/record.sh`), and re-recording changes every view's
  snapshots: the recording is the coordinator's, below.
- **Two sources on one time axis.** One map at a time; stacking two halves
  the rows each. Left for the user's verdict on the single map.
- **Read the value attribute from the run.** `results.json` does not carry
  the collection's value attribute, so the view names it per source
  (`latency_ns`, `wait_ns`, `lat_ns`, `duration_ns`), falling back to the
  first numeric attribute, which is where `omn` writes the value. Filed below.

## Privileged runs wanted

None that changes machine state. The recording core's handoff already asks
for (a fixture with GPU kernels, alone on the machine), with this workload in
place of `kernel 2000` if the heatmap is to show regimes rather than one band:

```console
make
target/debug/omn top --headless -d 30s -o tests/view_fixtures/gpu-phases -- \
    sh -c 'python3 tests/gpu_workload.py launches 3000; \
           python3 tests/gpu_workload.py compute 200; \
           python3 tests/gpu_workload.py memory 40 64; \
           python3 tests/gpu_workload.py launches 3000'
# then record.sh's `keep` rewriting, and
OMN_VIEW_BLESS=1 cargo test --test view_snapshots
```

It was 14 MB of `results.json` here, mostly the 6 000 `tiny` launches and
every view's other traced sources; `launches 1000` twice keeps the shape at a
third of the size.

## Decision lines for DECISIONS.md

- **The heatmap draws only `raw` events** (wave 19): never the aggregated
  emits, so a live run draws as a finished one, and a result kept as a
  summary or a histogram is said, not drawn along a time it does not hold.
  Reason: a `hist` has no time axis, and a view that drew from it at the end
  and from events while live would change shape when the run ended.
- **A kernel-bucketed row is drawn at its bucket's mean and over its drain
  window** (wave 19): `total_ns / weight`, which is inside the bucket and
  exact for one wait, spread over the columns its drain window covers in
  proportion -- `[ts - window_ns, ts]` for a row stamped at its drain's close
  (`net.socket`), from its stamp to the drain's close for one stamped at its
  first wait (`sched.runqlat`), cut to the run. A whole-run kernel row is
  not drawn. Reason: the low bound would draw every wait up to half an
  octave too low, and a point at the row's stamp would claim a moment the
  kernel never measured.
- **Latency rows are log2 and every screen row is used; density is
  logarithmic** (wave 19): an octave may be a fraction of a row more than the
  next, which the labels at powers of ten follow. Reason: whole rows per
  octave left a third of the map blank on the fixtures, and a linear density
  hides the one slow measurement beside a thousand fast ones, which is the
  spike the view exists to show.
- **A loss is shaded only where the run can place it** (wave 19): a live
  run's eviction before the first event kept, every other loss across the
  whole window marked "when unknown". Reason: placing a drop where it was not
  would draw a gap the run never had.

## Review

One round, seven findings; all taken.

- **Kernel rows drawn a window late** (defect, taken). The view read every
  drained row's `ts_ns` as its window's start; `collect::io::histogram`
  (`net.socket`) stamps at the window's close, and `sched::runqlat` at the
  row's first wait. `Stamp` now names each convention: a close-stamped row
  covers `[ts - window_ns, ts]`; a first-wait row covers from its stamp to
  its drain's close, bounded by the earliest first-wait end plus the window
  among the rows of the same drain (same result, same `window_ns` to the
  nanosecond) -- a bound that holds every wait it counted. Every spread is cut
  to the run, so the time axis no longer runs past its end.
  `heatmap--http-server` now puts the 807 µs call at 164-218 ms and the
  ~435 µs regime at 164-320 ms; the cpython-hot and runqlat snapshots shift
  by up to a window. A row whose `first_ns` was zero is stamped at the close
  by the collector and is then drawn from the close forward, at most to the
  same bound: no fixture has one. If the collectors ever stamp alike, the
  table goes; better, `results.json` could say how a source stamps (a
  request below).
- **Whole-run `io.latency` rows drawn as a burst at 0** (gap, taken): a row
  with `weight` and no `window_ns` is counted aside and said, "a histogram of
  the whole run, built in the kernel", with the measurements it stands for
  (`a_whole_run_histogram_is_said_not_drawn_at_the_start`).
- **The size test proved only no panic** (nit, taken): it now draws into a
  larger frame filled with a mark and checks every cell outside the view's
  area is untouched.
- **The loss strip's words clipped** (nit, taken): the shading gives way to
  the words, so they show whole at any width; tested at no margin.
- **"3 regimes" for five** (nit, taken): the header says "5 regimes, 3
  shown", and the cut keeps the slowest plus the two heaviest of the rest.
- **Shares over 100 % when zoomed** (nit, taken): a regime's weight is its
  rows' share inside the window, as the map counts them.
- **frame--gallery / frame--live re-blessed** (nit): again in this round,
  since both draw the heatmap; the request to core below stands.
- **The GPU drawing in the gallery** (nit, taken): moved to
  `docs/view/drawings/`, said to be hand-kept and unheld.

## Needs from another stream

- **Coordinator, the runner (`src/collect/*` drain loops, `src/main.rs`)**:
  "lost events shaded where they were lost" needs each drain round's drop count
  with its timestamp, kept beside the result (in `losses`, as a short series
  of `(ts_ns, dropped)`), not only the run's total. Until then the view shades
  every drop across the whole window.
- **core, `view/src/model.rs` and `view.json`**: carry each result's value
  attribute (the collection's `value_attr`), so a view reads a source's value
  by name from the run rather than from a table of its own. The heatmap's
  `value_attr()` is that table today.
- **Coordinator, the collectors and `results.json`**: say how a drained
  source stamps its rows (at the drain's close, or at the row's first event)
  in the result, or stamp them all alike. The heatmap keeps a per-source
  table (`Stamp::of`) that a new drained source would silently miss: it
  defaults to the close, which is `collect::io::histogram`'s.
- **core, `view/src/app.rs`, the header**: a long launched command squeezes
  the run's length into the cost corner with no space between them --
  `… launches 3000) │ 1.49sview: snapshot` on the phased GPU run at 120
  columns.
- **core, `docs/view/README.md`**: `frame--gallery` and `frame--live` draw
  the heatmap, so every change to this view re-blesses two of core's
  snapshots. Either draw them on the stub-free `system` view, or say in the
  README that a view stream re-blesses them.
- **lanes**: `⏎` here sends `Action::Goto { view: "lanes", focus }` with the
  slowest measurement's pid and tid: the lane of that tid is what it expects
  to open on.
- **request**: `⏎` on a measurement made for a request sends
  `Action::Goto { view: "request", focus }` with `focus.request` set to the
  request's id as its `req` key spells it (`a002`).
- **Coordinator, the fixtures**: the GPU recording above.

## The suite

While iterating: `cargo test -p omn-view --lib heatmap`, `cargo test --test
view_snapshots`, `cargo clippy -p omn-view --all-targets -- -D warnings`.

Before the last commit: one `make check`, red on two `--lib` tests outside
this stream's paths (`cpu::profile`'s machine profile, `mem::page_faults`'
weighed samples), both in the flaky tally, both passing alone. Then one
`cargo test --workspace --no-fail-fast`: 2 438 passed, 8 failed, 10 ignored,
with the machine's load average at 76-85 (the other streams' builds and
suites). Each failure rerun alone, once:

- passed alone: `a_profile_of_the_machine_finds_this_thread_for_the_cpu_it_burned`,
  `every_datagram_sent_to_a_closed_port_is_a_drop_with_one_reason`,
  `defect_1_the_first_reading_of_a_launched_target_is_of_the_program`,
  `a_layer_this_run_cannot_serve_is_reported_and_the_cheap_answer_stands`,
  `an_input_this_machine_cannot_serve_is_dropped_by_name_or_fails_the_question`,
  `the_walk_through_hands_over_what_the_runs_wrote_and_what_the_program_did`;
- failed again, both in the flaky tally (`docs/open-findings.md`) and both
  timing the cpu under that load:
  `cases::every_case_names_its_known_cause_both_ways` (`gcc-shell`'s on-cpu
  6.54 s against the reference's 7.50 s) and
  `runner::a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed`.
  Neither reads `view/`; worth one rerun on a quiet machine at the merge.

`cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt
--check`: clean. `tests/view_snapshots.rs`: 15 of 15.
