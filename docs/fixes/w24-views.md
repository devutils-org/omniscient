# The twenty-fourth wave, stream views

Branch `feat/w24-views`. The brief's item 1 first -- every view fills the
terminal the way `system` does -- then the views' open points. Merging
`main` half way turned one test of this stream's files red (the triage's
perf-map charging); that defect is fixed and said first.

## What landed

| Commit | What |
|---|---|
| `86c86ee` | `fix(view)`: **the flame charges own cost as the triage does.** Since the triage reads which frames a perf map named from the results' provenance (`named_by`, `f49d539`), its difference charges a sample to the innermost perf-map frame, else its leaf. The flame still charged the leaf, so `tests/view_model.rs`'s `the_flame_view_s_difference_is_the_triage_s` went red once triage merged (rerun alone: red each time). The flame reads `named_by` the same way (`Results::perf_map_named`), its own list says "the innermost frame a perf map named, else where it ends". `twice_the_work_...` now holds that the cpython pair's difference names `py::score`. |
| `a4e353e` | `feat(view)`: **every view takes the terminal's width and height.** The ledger's history column grows with the width (`GRAPH_CELLS` 28 -> 150, resampled to the column) and draws two rows high when the sheet and its detail leave the rows; the flows' ribbons and objects' column take the width past 118 cells; the flame's list of the heaviest takes the pane's rows (it stopped at 12) and its bars a fifth of the pane; the timebar's bars thicken up to five rows; the lanes' tall lanes deepen up to three rows a state with the rows the detail does not need; the request's waterfall bars thicken up to three rows. |
| `75746c3`, `f29af31`, `883d744` | `feat(view)`, `test(view)`: **the sized snapshots** -- `every_view_fills_the_terminal` in `tests/view_snapshots.rs` draws flame, ledger, request, timebar, heatmap, lanes, wake and flows on their own workloads at 80x24, 160x48 and 240x60, holds each to a snapshot, and fails a drawing that does not reach the last quarter of the rows and of the columns. Under 100 cells the timebar's bars take the suspect column (the first suspect is named above them). **`Update::Described { index, value_attr, row_stamp, drains }`** in `view/src/feed.rs`, applied after the first events too, each drain kept once. **`Key::Dir { dir }`** in the mirror, printed `dir:read`. |
| `bb25ed9` | `test(view)`: **`--against` and focus in the standard set**: `every_view_against_another_run` (flame and ledger, cpython-hot against cpython-hot-heavier, 120x40 and 240x60) and `every_view_focused` (each of the eight views narrowed to a second thread of its workload, or request `a003`). |
| `19a43c7` | `test(view)`: **the usage gradient on a dark and a light terminal, in 256 colours** (`tests/view_gradient.rs`): each step of `usage::RAMP` as the 256-colour cube draws it, WCAG 2 contrast against four backgrounds, the 3:1 floor for graphical objects; the strips drawn into `docs/view/gallery/gradient--*.ansi` for `cat`. A defect found, filed below; the light check is an ignored test until the ramp changes. |
| `7b766c6` | `feat(view)`: **the request view's bytes**: what the server read off the socket and what it queued, from `net.request`'s `rx_bytes` / `tx_bytes`, under the network's time; `?` where its start was not seen, and "not in this recording" for one made before the count. |
| `74da698` | `feat(view)`: **flows' socket -> interface edge**: `net.socket`'s rows carry the interface of the socket's cached route as a key; the interface the machine's column measured says `◂ N sockets`, as a device says its files, and the footer no longer says sockets are not tied to interfaces when they are. |
| `045493d` | `feat(view)`: **the ledger asks `cpu.oncpu_time` with `raw`**: user and system apart are on the events only, and a live run draws the line from its events. A backend that keeps events without the split is named ("bpf:raw_tp does not split them; fs:proc does"). `docs/view/README.md`'s row updated, since `tests/view_model.rs` holds the table to the needs. |
| `7118061` | `test(view)`: **`sqlite-latency` recorded with its blocked rows' events**: `record.sh` adds `raw` to the `blocked` request `omn latency` dumps. The timebar splits the blocked time by state; the recording is also the first to hold a drain. |
| `90e1f72` | `style(view)`: rustfmt, and one clippy lint in the lanes. |

## Shown vs reasoned

Shown, on this machine:

- **The views fill the terminal.** Measured on the drawn text (how far down
  and right the last non-blank, non-border cell is), before -> after: the
  timebar at 240x60 reached **68 %** of its rows, now **82 %**; the lanes at
  160x48 left **11** blank rows, now **1**; the ledger's history is **64**
  cells at 160 columns and **144** at 240 (it was 28 at both), and on a tall
  terminal two rows high; the flame's list at 240x60 holds **29** functions
  (it held 12) and leaves **2** blank rows (it left 17). Every one of the
  eight views reaches at least 75 % both ways at all three sizes (the test).
  At 80x24 the timebar's bars are **42** cells (they were 22).
- **The request's bytes**, held on screen for all **16** requests of the
  request workload (`request_draws_each_requests_bytes_as_its_server_wrote_them`):
  **51 B** read for a plan with no two-digit figure, **52 B** for `30:0` and
  `0:32`, the length of `GET /?lock=<ms>&reads=<n>&body=0 HTTP/1.1\r\nHost:
  omn\r\n\r\n`; **59 B** queued, the 57-byte header plus the 2-byte body
  `examples/vertical/server.c` writes.
- **The socket -> interface edge**: on the flows workload, whose one socket
  connects to `127.0.0.1`, `lo` is tied to **1 socket** and no other
  interface to any (`flows_ties_the_socket_to_the_loopback`); on
  `file-serving`, `lo` again.
- **The gradient**, contrast ratio of each step (index, then black / #1e1e1e
  / white / #fdf6e3): 34 7.13 / 5.66 / **2.94** / **2.73**; 106 8.16 / 6.48 /
  **2.57** / **2.39**; 178 10.02 / 7.96 / **2.10** / **1.94**; 208 8.72 / 6.92
  / **2.41** / **2.23**; 202 6.89 / 5.47 / 3.05 / **2.83**; 160 3.89 / 3.09 /
  5.40 / 5.01; the neutral 67 5.57 / 4.42 / 3.77 / 3.49. It reads on a dark
  terminal; on a light one the four first steps are under 3:1. The ramp
  `28, 64, 100, 136, 166, 160` holds 3:1 on all four, green to red in order
  (hues 120, 78, 60, 46, 27, 0).
- **sqlite-latency's blocked with its events**: the timebar's suspect is now
  `blocked: uninterruptible`, **240 ms**, **71.9 %** of the program's
  life, 49 % from one stack whose leaf is `fdatasync` -- the commit waiting
  for the disk, which is what case 5 builds. `blocked` carries **72** events.
- **A recording holding a drop**: the new `sqlite-latency` holds
  `sched.futex`'s drain, **58** skipped runs at one drain ("the kernel
  skipped a run of this program because one was already running on that
  cpu"; the count is the machine's).
- **The flame and the triage agree**: both name `py::score` alone as what
  moved between cpython-hot and cpython-hot-heavier, the function the heavier
  input makes slower by construction (`cases/cpython-interpreter/hot.py`).
- Tests run: `cargo test -p omn-view` 199 of 199; `view_snapshots` 20 of 20,
  `view_truth` 13 of 13, `view_model` 7 of 7, `view_live` 5 of 5,
  `view_timebar` 1 of 1, `view_gradient` 4 of 4 and 1 ignored; clippy
  `--workspace --all-targets -D warnings` and `fmt --check` clean, after the
  merge of `main` at `9e429d0`.

Reasoned only:

- That `Update::Described` keeps a drawn drop when the runner's list no longer
  holds it: shown on synthetic updates, not on a live run, since `omn top`
  does not send it yet (below).
- That the ledger's `raw` costs little: one event per thread per 250 ms poll.
  Not measured.

## Not done, and why

1. **The file -> device drawn ribbon** in flows. The textual edge (`◂ N
   files`) stands. A ribbon proportional to the files' bytes would draw a
   volume that is not the device's: those bytes went through the page cache,
   and the device's are fewer (file-serving: 115 MiB of files, 15.5 MiB on
   `259:0`). It needs the junction drawn as two layers; not started.
2. **A placed drop on screen.** The recording that holds a drain holds it on
   `sched.futex`, which no view places in time; the heatmap places drops only
   on its own four sources. A recording with a drop on `sched.runqlat`,
   `io.latency`, `net.socket` or `gpu.kernel` needs a load that overruns the
   ring, which a workload cannot promise.
3. **`omn top` sending `Update::Described`**: `src/main.rs` is stream run's
   (below).
4. **The `raw` panels `system` could keep (`cpu.util`, `gpu.mem`)**, the
   `process` items, `system` split by `dir`, the mouse in Ghostty and tmux:
   `system.rs`, `process.rs` and `trust.rs` are stream top's, the mouse needs
   a person at a terminal (item 9 of "Needs privilege").
5. `system` and `process` are not in the sized snapshot set: their fill is
   stream top's to hold (`tests/view_top_*.rs`).

## Privileged runs

- `make` (its `sudo -n setcap`) then `tests/view_fixtures/record.sh
  sqlite-latency`: done, in this worktree.
- The mouse, by a person: `target/debug/omn top --replay
  tests/view_fixtures/http-server` in Ghostty, then inside `tmux` with
  `set -g mouse on`, a click on a line and a right click back in each view.
- The gradient by eye: `cat docs/view/gallery/gradient--usage-on-white.ansi`
  and `gradient--proposed-on-white.ansi` in a light-themed terminal, and both
  inside a 256-colour tmux (`tmux -2`).

## Shared files touched

- `docs/view/README.md`: the ledger's `cpu.oncpu_time` row, `raw,` added to
  its emits; `tests/view_model.rs` holds the table to the code, so the row
  had to move with the need.
- `docs/view/gallery/`: the snapshots, as tests write them. Re-recording
  `sqlite-latency` and the flame's charging re-blessed `system--*` and
  `process--*` snapshots too (stream top's views): a merge with top's
  changes there is resolved by re-blessing (`OMN_VIEW_BLESS=1 cargo test
  --test view_snapshots`).

## Decision lines for `DECISIONS.md`

- A view's layout scales with the terminal: every growable element (a
  history, a bar, a lane, a list) takes the rows and columns the fixed lines
  leave, up to a bound that keeps it readable; `tests/view_snapshots.rs`
  holds each view at 80x24, 160x48 and 240x60 to reaching the last quarter of
  both.
- The flame's own cost is charged as the triage's: the innermost frame a perf
  map named, else the leaf. The two read one provenance and must name the
  same functions -- where the provenance exists. For stacks without
  `named_by` (`sched.offcpu`'s) the triage reads the cited maps back from
  `/tmp`; the view does not, because it draws what the recording says and a
  map on the viewing machine may not be the run's. It charges the leaf there
  and says so in the pane's aside ("own cost at the leaf: these stacks do not
  say which frame a perf map named"). The divergence closes when
  `sched.offcpu` writes `named_by` beside its stacks.

## Review

- **flame.rs, own cost of stacks without `named_by`** (gap): right. Kept
  the view from reading perf maps back (see the decision line), and made the
  divergence said rather than only inferable from the list's header: a
  result with stacks, no `named_by` and a perf map in its provenance or its
  names note now puts "own cost at the leaf: ... (`omn triage` reads the
  maps back)" in the flame pane's aside. Held by
  `stacks_without_provenance_named_by_a_perf_map_say_where_own_cost_went`;
  the `flame-*--workload-flame` and `flame--workload-timebar` snapshots
  re-blessed, since that recording's off-cpu stacks cite `perf-2620696.map`.

## Needs from another stream

- **sched.offcpu** (`src/collect/sched/`): write `named_by` and `perf_maps`
  beside the stacks as `cpu.profile` does; the flame and the triage then
  charge off-cpu own cost alike, and the aside goes away.


- **top** (`view/src/trust.rs`): `usage::RAMP`'s first four steps are under
  3:1 on a light terminal (178 is 2.10:1 on white). Proposed: `28, 64, 100,
  136, 166, 160`, 3:1 or more on black, #1e1e1e, white and #fdf6e3, in the
  same green-to-red order; then drop the `#[ignore]` on
  `tests/view_gradient.rs`'s `the_ramp_reads_on_a_light_terminal`.
- **top** (`view/src/views/system.rs`): on `sqlite-latency` the processes'
  trust line reads `ON LOCKS -58 lost`, which reads as a negative count.
- **run** (`src/main.rs`, `Tap::described`): send
  `omn_view::feed::Update::Described { index, value_attr, row_stamp, drains }`
  on every change instead of restarting the view only before the first
  events; the restart bridge can then go.
- **coordinator** (`docs/view/request.md`, "What it does not show"): "Bytes on
  the network" is stale -- the view draws what the server read and queued.
