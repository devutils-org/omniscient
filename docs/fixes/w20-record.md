# The twentieth wave, stream record

Item 2 of `STATE.md`'s "What remains", phase B's recording half: one
re-recording of every fixture on merged phase A, a recording of each view's
workload, every snapshot redrawn, each view held to its workload's manifest,
and the gallery's README rewritten with the user's verdict. Branch
`feat/w20-record`, on `main` at `777d6dd` (every phase A stream merged).

## What landed

**One re-recording** (`tests/view_fixtures/record.sh`, `7cf6e93`), through
`omn top --headless` with every view's needs, on a binary carrying the
capabilities (`make`), in 2 min 37 s:

- The seven realistic runs again. `cpython-hot` and `cpython-hot-heavier` now
  run under `-X perf`, so their Python functions are named through the
  interpreter's perf map (the "CPython run with its perf map").
- New beside them: `file-serving` (the file server of
  `docs/view/gallery/flows-run/`, whose `record.sh` now takes an output
  directory), `vertical-abandoned` (`examples/vertical-abandoned.yml`),
  `many-requests` (300 requests on the vertical server, four causes in turn;
  the server is built from `examples/vertical/server.c` with its 64-entry plan
  limit raised by `sed`, into `target/`, the source untouched) and
  `system-wakeup` (every wakeup on the machine for one second; five seconds
  was 13 MB, one is 2.2 MB).
- A recording per view's workload, `workload-<view>` for the nine views,
  launched as each manifest's `run` says from the repository's root (the
  request server built first, its plan read from its session). `workload-lanes`
  is the GPU fixture: twelve `spin` kernels on the device, served by
  `vendor:cupti`. `process`'s workload is not recorded here: the process
  stream records it into `docs/view/gallery/process-run/`.
- 20 fixtures, 22 MB in all (`many-requests` 4.6 MB, `sqlite-commits`
  3.3 MB, `system-wakeup` 2.2 MB the largest).

**Every snapshot redrawn** on them (`OMN_VIEW_BLESS=1 cargo test --test
view_snapshots`, and the views' own tests with the same variable), except
`process--*`, which is the process stream's: the existing ones are left as
they were and the new fixtures have none yet (see "Not done").

**`tests/view_truth.rs`** (new, `92256d1`): each of the nine views drawn on
its own workload's recording, the manifest's numbers read off the screen the
way a user reads them, within each figure's `trust`:

| View | What is held | Drawn on this recording |
|---|---|---|
| flame | `heavy`/`light` and `heavy`/`medium` within 20 % of 4 and 2, through the view's own search; `nap` off cpu at least 0.60 s | 53.9 %, 15.0 %, 28.6 % (3.59, 1.88); `nap` 23.0 % of 2.61 s = 0.60 s |
| timebar | the worker selected: on cpu 0.90 s, asleep 0.80 s, of it 0.40 s in futex(2), runnable at least 0.15 s | 902 ms, 882 ms, 405 ms, 722 ms |
| wake | each relay woken by the one before it, 100 %, 180 to 300 times | 300, 299, 299; the ring and no backward edge |
| flows | the socket's bytes exact | 4.00 MiB sent, 1.00 MiB received |
| heatmap | 600 receives in a ~1.5 ms regime, 50 in a ~12 ms one, the slow phase's span 0.60 to 0.78 s | ~1.58 ms (600), ~12.1 ms (50), 539 ms to 1.22 s |
| ledger | cpu at least 0.5 s, memory grown 64 to 70 MiB, the socket's bytes exact | 640 ms, +64.3 MiB, 2.00 MiB out, 256 KiB in |
| system | two processes at 1.0 s of cpu each; the disk at least 16 MiB; loopback at least 16 MiB | 1.05 s and 1.03 s; 16.2 MiB; 36.8 MiB |
| lanes | lanes a, b, c first awake in order, awake shares within 25 % of each other; 12 kernels and a `gpu0 … spin` lane when the device served | 21.6 %, 21.4 %, 20.8 %; 12 kernels |
| request | 16 requests; each plain one under 1 ms on the cpu, each lock wait 30 to 39 ms on `lock/sleep`, each cold read on `disk`, each paused client 40 to 52 ms on `net in`, in the plan's order | all 16 as planned |

**Each view after a click** (`every_view_after_a_click` in
`tests/view_snapshots.rs`): one `<view>-click--<fixture>` snapshot per view,
the click landing on what the view drew (a process row, a frame, a ledger
line, a request, a bar, a heatmap cell, a lane, a woken thread, a file), each
read against the plain snapshot: every one moved the selection to what was
clicked. `flows` is clicked on `file-serving`, because the flows workload's
single socket is already the one selected.

**The gallery's README** (`ed52743`): retitled for the nine views kept, the
`Keep?` column "yes (2026-10-04)" on every row, the workload snapshot first in
each row, a section on the recordings and on `view_truth.rs`, the frame's
snapshots where they are now drawn, the links and the mouse.

## Shown versus reasoned

Shown: every number in the table above, by `cargo test --test view_truth` (9
passed); `cargo test --test view_snapshots` 16 of 17 (the 17th is `process`,
below); `cargo test --test cli` 27 passed (the gallery README); clippy with
`-D warnings` and `cargo fmt --check` clean.

Reasoned, not shown:

- That a re-recording draws the same screens within their measurement. One
  recording was made; the bounds in `view_truth.rs` are the manifests'
  trust, not this run's numbers, so a re-recording should pass them, but none
  was made to see it.
- That a terminal delivers the clicks the `-click--` snapshots answer: the
  harness sends the event the way crossterm reports it.

## What I could not do, and why

- **18 of the view crate's unit tests fail on the new fixtures**
  (`cargo test -p omn-view`: 155 passed, 18 failed). They are in
  `view/src/views/*.rs`, which this stream does not own, and each holds a
  figure of the old recordings rather than a property:
  - hard-coded pids of the old recordings: `system`'s
    `every_thread_any_source_saw_is_listed`, `sorting_reorders_the_list`,
    `enter_opens_the_selected_process_in_its_ledger`,
    `the_target_is_in_the_list_with_its_cpu` (2758953 and its threads), and
    `timebar`'s `the_gallery_holds_the_view_live_chosen_and_rescaled`
    ("2758972 python3");
  - durations of the old recordings: `request`'s
    `what_sets_each_request_apart_is_its_cause` ("lock/sleep +40.1ms",
    "disk +8.27ms") and `an_undrained_response_is_unknown_not_gone_when_done`
    ("out ≥28.5ms"), `heatmap`'s `a_row_stamped_at_its_close_is_drawn_over_the_window_before`
    (the slowest call's place), `flame`'s
    `twice_the_work_moves_everything_absolutely_and_little_in_shape` (it
    expects every moved function up; under `-X perf`, 41 of 45 are);
  - what phase A changed in the sources, which the old recordings did not
    carry: `gpu.kernel` is now served, not refused (`heatmap`'s
    `snapshots_beyond_the_default`, `ledger`'s `a_refused_line_says_why`);
    `cpu.oncpu_time` is served on the machine run and per thread
    (`system`'s `the_machine_is_drawn_panel_by_panel` expects it refused;
    `ledger`'s `a_focus_narrows_every_per_task_line` expects `cpu time`
    blank on one thread; `timebar`'s
    `a_residue_that_cannot_fail_is_not_printed_as_one` finds a residue on an
    on-cpu-bracketed bar of `http-server`); `io.syscall` on `sqlite-commits`
    no longer has a 512-bucket histogram (`ledger`'s
    `syscall_time_is_exact_and_its_distribution_drawn`); the machine run's
    title (`ledger`'s `a_live_run_without_tasks_does_not_claim_the_machine`);
  - one worth reading before it is re-pinned: `system`'s
    `every_timeline_of_the_machine_ends_where_the_run_does` asserts the
    target's `cpu.oncpu_time` timeline starts at 0, and on the new recording
    it starts at 16.4 ms -- the first recording that carries `first_ts_ns`,
    so the view may now move the target's own timeline, which the test says
    it must not; and `flame`'s `off_cpu_splits_by_thread_and_narrows_to_a_focused_one`
    gets no tree from the off-cpu needs on its fixture.
  Filed below for the view crate's owner.
- **The `process` snapshots**: `docs/view/gallery/process--*` is the process
  stream's. The existing ones are left as committed (drawn on the old
  recordings by the stub) and the thirteen new fixtures have none, so
  `view_snapshots::process` fails here until the process stream blesses them
  on these recordings.
- **Bytes per file in `flows` and in the ledger, on a workload's
  recording**: `omn top`'s needs ask `io.syscall` as a summary and a
  histogram, without `raw`, so on `workload-flows` the flows view says "bytes
  per file: not in this run" and the ledger "bytes read and written: on the
  raw rows only, which this run did not keep". The manifests' file bytes
  (8 MiB, 1 MiB, 4 MiB) are therefore not held in `view_truth.rs`; the
  socket's are, exactly. A need with `raw` is the view's table
  (`docs/view/README.md`) to change; then `file_bytes` from
  `tests/view_workloads.rs` is the check to add.
- **`--against` and focus snapshots in the standard set** (the view stream's
  request): not done for time. `snapshot::draw_with` is there; the ledger's
  two stay in `ledger.rs`.
- **The heatmap's two fast phases** are one regime on screen (`~1.58ms
  (600)`), as they should be; the test holds the pair of boundaries of the
  slow phase, not the fast phases' own.

## Privileged runs wanted

None beyond what `make` grants. To re-record (about three minutes, the GPU
in use by `workload-lanes` for under a second):

```console
make && tests/view_fixtures/record.sh                 # all twenty
make && tests/view_fixtures/record.sh workload-lanes  # one
OMN_VIEW_BLESS=1 cargo test --test view_snapshots     # then look at the diff
```

## Decision lines

- **A view's truth is read off its screen**, not off its model: most views
  keep their model private, and the screen is what the user reads. Cost: a
  wording change in a view breaks `view_truth.rs` with a clear message
  ("no line with `…`"), which is the point.
- **The workload recordings are fixtures like the others**, so every view is
  drawn on every workload (117 new snapshot pairs, and 9 after a click). Cost:
  22 MB of fixtures in the tree; `system-wakeup` cut to one second for that.

## Needs from another stream

- **The view crate's owner (`view/src/views/*.rs`)**: the 18 unit tests
  above re-pinned to the new recordings, or better, rewritten as properties
  (pids read from the data, not written in the test); and
  `every_timeline_of_the_machine_ends_where_the_run_does` read first, since
  it may be a real shift of the target's timeline by `first_ts_ns`.
- **process (`docs/view/gallery/process--*`)**: bless `process--<fixture>`
  on these recordings, the thirteen new fixtures included.
- **view (`docs/view/README.md`, the needs table)**: `io.syscall` with `raw`
  for flows and ledger, so a workload's bytes per file are drawn; then
  `view_truth.rs` holds the file bytes too.
- **docs (`examples/vertical/server.c`)**: `MAX_PLAN` raised from 64, so a
  plan of a few hundred requests does not need the `sed` in `record.sh`.
- **coordinator**: the `frame--help` and `frame--narrow` snapshots are drawn
  on `process`; once the process stream lands its view they change, and are
  re-blessed at that merge.
