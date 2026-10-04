# The twentieth wave, stream process

The `process` view ("Decided before this wave": one process at a glance, the
first screen with a target). Branch `feat/w20-process`, one commit before
this one (`8ad2b96`), on `main` at `efc5b85` (view and workloads merged).

## What landed

**`view/src/views/process.rs`**, replacing the stub. Nine tiles in a 3×3
grid, each one view's essence for the target, a headline number and a
colour, each opening its view:

| Tile | Opens | Essence |
|---|---|---|
| time | timebar | the first suspect and its share of the program's bar, from `timebar::sheets` and `omn latency`'s verdict, and a 24-cell strip of the bar |
| cpu | flame | the hottest own code on cpu (caller › callee) and its share; the same off cpu |
| cost | ledger | cpu time (the `lifetime`'s `cpu_ns`, else `cpu.oncpu_time`), rss peak, faults |
| request | request (focused on it) | the slowest request, its duration and largest state; `no request declared` in dim when `net.request` is refused |
| latency | heatmap | max, a strip of the slowest per slice, count and median, of the source the heatmap opens on |
| threads | lanes | thread count and a strip of context switches |
| wake | wake | the top waker of the target's threads (a thread by name, or an irq context) |
| flows | flows | socket bytes in and out, device bytes read and written |
| gpu | lanes | the kernels' share of the wall clock, device memory peak |

- **Colour is what was found**: the suspect's state colours the time tile's
  headline and gives the tile that explains it (on cpu/sleep/blocked → cpu,
  run queue → threads, lock → wake, I/O/network → flows, else cost) a thick
  border in the same colour -- never a tile the run holds nothing for. Every
  figure is in the trust style; each tile's badges are on its last row so
  the headline stays readable.
- **Absence is said**: `not in this run` with what the tile needs, or
  `<source> refused` with the reason.
- **Keys and mouse**: `↑↓`/`jk` a row, `hl` a tile, `home`, `enter` opens
  (a `Goto` the frame keeps in its history); `regions()`/`select()` give
  click to select, click again to open, right click back.
- **The recording** `docs/view/gallery/process-run/` (results.json and
  view.json, 2 MB, paths rewritten to `/omniscient`), made by `omn top
  --headless` from `view/workloads/process/workload.py` asking every view's
  needs; its snapshot `process--process-run`.
- **`docs/view/process.md`**: the tiles, the colour rule, the keys, what a
  stack is charged to, the recording against the manifest, what it does not
  show.
- Every `process--<fixture>` snapshot re-blessed; `frame--help` and
  `frame--narrow` (drawn on `process` since the view stream) re-blessed too.

## Shown versus reasoned

Shown, by tests in `process.rs` (6, all passing):

- `the_tiles_find_what_the_workload_did`, against
  `view/workloads/process/manifest.json` on the recording: the cpu tile
  names `crunch` first (`crunch › burn 71.0%`); the rss peak is at least the
  32 MiB held (46.7 MiB); the socket bytes are exact (`net ↓256KiB
  ↑1.00MiB`); the request tile says no request is declared; the gpu tile
  says no kernel; at most one tile is marked, and exactly when the time tile
  names a suspect.
- `each_tile_opens_its_view_by_key_and_by_click`: every tile by keys opens
  its view and `⌫` comes back to `process`; a click selects the wake tile, a
  second opens `wake`, a right click comes back.
- `a_tile_without_its_sources_says_so_and_a_refusal_says_why`, unit tests of
  `charged_to` and `request_cause`, and the snapshot on the recording.
- `cargo test -p omn-view`: 179 passed (the app's every-region and
  twenty-clicks-per-view tests included); `--test view_snapshots` 16 and
  `--test view_model` 5 passed; clippy `--workspace --all-targets -D
  warnings` and `cargo fmt --check` clean.

Reasoned, not shown:

- **The suspect on this workload is `sleep`, not the lock.** The timebar's
  reading of the recording does not split the main thread's 0.25 s on the
  lock out of its sleep (the run holds `sched.futex` as a summary only). I
  did not investigate whether the timebar should; on `vertical-requests`,
  where a decomposition names it, the tile reads `▶ lock 99.8%`.
- **The disk bytes** (`↑4.07MiB` for a 4 MiB file) are taken as the file
  plus the filesystem's own writes; not checked block by block.
- **The tiles in a real terminal** (colours, the thick border): seen only
  in the `.ansi` snapshots.

## What I could not do, and why

- **Opening `process` first on a live `--pid` or launched run**: `src/main.rs`
  (budget's), already filed by the view stream.
- **The ledger's own sheet and the flame's own "hottest own"**: both are
  private to `ledger.rs` and `flame.rs`; the tiles read the same sources
  their own way. A public `ledger::cost_lines(data)` and
  `flame::hottest(data)` would make the tiles the views' own words (filed
  below).
- **Kernel frames in a native profile**: a stack that ends in the kernel is
  charged to two kernel functions (`sqlite-commits`: `btrfs_check_leaf ›
  __btrfs_check_leaf`), because frames carry no user/kernel mark the view
  can read.
- **The gallery's `Keep?` column and README rows** for `process` are
  record's files (`docs/view/gallery/README.md`).

## Privileged runs wanted

None beyond what was done: the recording ran here with the capabilities
`make` grants (`sudo -n setcap`, listed). To re-record:

```console
make
target/debug/omn top --headless -d 30s -o docs/view/gallery/process-run \
  -- python3 -X perf view/workloads/process/workload.py
sed -i "s|$PWD|/omniscient|g" docs/view/gallery/process-run/*.json
OMN_VIEW_BLESS=1 cargo test -p omn-view --lib views::process
```

And one for the user at a terminal: `target/debug/omn top --replay
docs/view/gallery/process-run`, then click a tile twice and right click.

## Decision lines for DECISIONS.md

- **A tile's colour is the suspect's state, on the tile that explains it**
  (cpu, threads, wake, flows or cost by the state), never a threshold; a
  tile with nothing to read is never marked. Cost of leaving it open: none.
- **A stack is charged to its two deepest frames of own code, caller
  first** (interpreter frames when present), so a shared helper does not
  hide whose work it was. It differs from the flame's "hottest own"; the
  alternative is to make the flame's public and accept `libpython+0x...` as
  the answer on an interpreter.
- **A program that declares no request is not an error on this screen**:
  the request tile says it in dim with the refusal's reason.
- **Trust badges go on a tile's last row**, not beside the headline.

## Needs from another stream

- **record (`docs/view/gallery/README.md`, `tests/view_snapshots.rs`)**: add
  `process` and its `process-run` recording to the gallery's table with
  `Keep?` "yes (2026-10-04)"; the frame snapshots `frame--help` and
  `frame--narrow` were re-blessed here because they are drawn on `process`.
- **budget (`src/main.rs`)**: open `process` first on a live `--pid` or
  launched run (the view stream's request, still open).
- **ledger's and flame's owner, next wave**: a public reading of the
  ledger's cost lines and of the flame's hottest function, so the tiles say
  exactly what the views say.
- **timebar's owner, next wave**: whether a `sched.futex` summary per task
  should split a lock out of sleep on the program's bar (the process
  workload's 0.25 s on a lock reads as sleep).
