# The twenty-first wave, stream process

The `process` view as the system view for one process ("Decided before this
wave"). Branch `feat/w21-process`, on `main` at `156f1c5` (no other stream
merged when it started), commits `3925711` (the view and its snapshots) and
`cb6729f` (the document, the workload's manifest and session), then this
handoff.

## What landed

- **`view/src/views/process.rs`, rewritten.** The tile grid goes. One header
  (`process-main  pid 1476208 · 3 threads · 2.37s · omn costs ≥10.7% cpu`),
  one line per resource -- CPU, Memory, Disk, Network, GPU, Energy, Time --
  each a 24-cell sparkline over the program's life and its figures (now,
  mean, peak, share of the machine where the capacity is known), then
  `▶ suspect: <state> (<what names it>)  <share>` from the timebar's verdict,
  and under it which lines are kept neutral and why.
- **Pages**: CPU → `flame`, Memory → `ledger`, Disk → `heatmap` (`flows`
  when `io.latency` is not in the run), Network → `flows`, GPU → `lanes`,
  Energy → `system`, Time → `timebar`, the header's cost → `ledger`, the
  suspect → the page that explains its state. `regions()` give one row per
  line: a click selects, a second opens, a right click comes back.
- **Usage gradient** (`usage(fraction)`: green, yellow past half, red past
  four fifths) by the share of a capacity that exists: a live run's cores
  (`available_parallelism`) and `MemTotal`, the GPU's engine time. Disk and
  network bytes, energy, and CPU and memory in a recording keep one neutral
  hue (`NEUTRAL`, blue) and the screen says why. Trust badges stand after
  the figures and never dim the hue.
- **Absence in place**: `not asked: needs io.latency`, dimmed; a refusal in
  the refused colour with its reason; `no kernel launched`, `no block request
  of the process`, `no socket bytes` when the source is there and empty.
  Never a zero.
- **A machine run** (a recording without a `lifetime`) no longer claims a
  target: `no target: this run measured the machine ... every task's, not
  one process's`.
- **Energy** is the machine's, `not apportioned`, in a recording; live,
  `≈` the machine's watts by the process's share of the busy cores
  (`cpu.util` × cores), said `estimated`.
- **omn's cost** in the header is the frame's own computation
  (`app::run_costs`, share of one cpu over the run, `≥` where an in-target
  cost is unknown), so the two figures agree.
- **Workload**: `view/workloads/process/manifest.json` gains `phases` (per
  line, the known value) and `not_exercised` (GPU, Energy, a recording's
  capacity); `session.yml` asks `io.latency` (label `disk`) for the Disk
  line. `workload.py` is unchanged.
- **`docs/view/process.md`** rewritten: the lines, what each reads and opens,
  the colours, keys, the recording held to the manifest, what it does not
  show.
- **Snapshots re-blessed** (in `docs/view/gallery/`, record's directory, a
  direct consequence of the view): every `process--*` and `frame--help`,
  `frame--narrow` (drawn on `process`). Nothing else in the gallery changed.

## Shown versus reasoned

Shown, by the tests in `process.rs` (8, all passing):

- `the_lines_find_what_the_workload_did`, on the existing recording
  `docs/view/gallery/process-run/` against the manifest: the CPU mean is the
  program's own cpu (0.89 s) over its 2.37 s life, at least the 0.75 s
  burnt; the CPU sparkline's integral is the polled cpu within 0.9× and
  +0.6 s, its peak at most 1.5 cores (one thread burns); the memory peak is
  at least the 32 MiB held and grows; the network rates are the manifest's
  bytes over the life exactly, the sparkline's integral within a byte of
  1.25 MiB; the disk sparkline holds at least the 4 MiB file and a p99; GPU
  says `no kernel launched`, dimmed; energy says the machine's; CPU and
  Memory are neutral and the note says so; the header names `process-main`
  and omn's cost.
- `each_line_opens_its_page_by_key_and_by_click`: all nine targets by keys
  and back; Network, Time and the header's cost by two clicks and back.
- `a_line_without_its_sources_says_so_and_a_refusal_says_why`: an empty run
  draws every line `not asked: needs ...`, dimmed, no value, no neutral
  note; a refused `net.socket` says `refused: needs CAP_BPF`.
- `usage_takes_the_gradient_by_the_capacity_and_trust_keeps_the_hue`: on
  the recording made live, CPU takes the gradient by the cores, keeps it
  under the `provisional` mark (not dimmed), says its share of the machine;
  energy is estimated; network stays neutral.
- `the_manifest_s_phases_are_the_program_s_figures`: the phases cannot
  drift from the fields the program reads; every line is a phase or
  `not_exercised`.
- `an_amount_is_spread_over_the_cells_it_covers`, `mem_total_is_read_in_bytes`.
- `cargo test -p omn-view`: 184 passed; `--test view_snapshots` 17,
  `--test view_truth` 6, `--test view_model` 10 passed; clippy
  `--workspace --all-targets -D warnings` and `cargo fmt --check` clean.

Reasoned, not shown:

- **The gradient on a real terminal**, live: seen only through the test's
  styles and the `.ansi` snapshots (which, on recordings, are neutral).
- **The workload's new phases on a new recording**: the recording is the
  twentieth wave's; the session's added `io.latency` and the manifest's
  phases were not run (privileged; below). The Disk line already reads
  `io.latency` from that recording, because `omn top` asked every view's
  needs.
- **Energy figures**: 127 W average on this recording is the machine's,
  with six other agents running; nothing checks it against the workload.
- **The suspect stays `sleep`**, not the lock, as in the twentieth wave:
  the timebar does not split the 0.25 s on the lock out of the sleep.

## Not done, and why

- **A recording's capacity.** No result carries the machine's cores or
  memory, so a replay cannot colour CPU and Memory; reading this machine's
  would be a guess about another. Requested below.
- **Disk and network fill.** A device's busy time and a link's speed are not
  read by any source the views ask for; those lines stay neutral as the
  brief allows.
- **The palette.** `usage()` and `NEUTRAL` are local to `process.rs`
  (three steps, sixteen colours) until views lands the gradient in
  `trust.rs`; then replace them by its function (two call sites:
  `Row::hue`, the test).
- **The request page** is no longer opened from `process` (no line for it in
  the sketch); the frame's `4` opens it.
- **A new recording** of `process-run`: privileged, handed to record.

## Privileged runs wanted (for record)

```console
cd /home/moha/repos/omniscient   # after the merge
make
target/debug/omn top --headless -d 30s -o docs/view/gallery/process-run \
  -- python3 -X perf view/workloads/process/workload.py
sed -i "s|$PWD|/omniscient|g" docs/view/gallery/process-run/*.json
cargo test -p omn-view --lib views::process          # held to the manifest
OMN_VIEW_BLESS=1 cargo test -p omn-view --lib views::process::tests::snapshot
cargo test --test view_workloads process_carries_every_cost_at_once
```

## Decision lines

- The process view's lines read the program's life (`lifetime`), not the
  run's window, as their time axis; the run's window when there is none.
- A recording is drawn without capacity (neutral hue, said) rather than with
  the viewing machine's.
- Energy for one process is shown only as an estimate, live, by its share
  of the busy cores; otherwise the machine's, said so.
- omn's cost in the process header is the frame corner's figure, not a
  share of the program's cpu.

## Needs from another stream

- **views** (`view/src/trust.rs`): the usage gradient and a neutral hue as
  public items (e.g. `palette::usage(fraction) -> Color`, `palette::NEUTRAL`)
  readable on dark and light and in 256 colours; `process.rs` will call them
  in place of its local `usage` and `NEUTRAL`.
- **run** or **data** (whoever owns what `omn top` writes beside a run): the
  machine's capacity in the recording -- online cpus (and the cgroup's cpu
  quota), `MemTotal` (and the cgroup's `memory.max`), each GPU's VRAM total,
  link speeds where known -- e.g. in `view.json`'s `Beside` (views owns
  `model.rs`) or as `Results::fields` of a machine source; `Capacity::of` in
  `process.rs` reads it when it exists.
- **record**: the new `process-run` recording with the session's
  `io.latency` and the snapshots re-blessed on it (commands above); the
  gallery's `process--*`, `frame--help` and `frame--narrow` were re-blessed
  here.

## Review

Merged `main` at `c8aa662` (views and docs landed) first; it merged clean.

- **"not asked" said of a source that was asked** (`view/src/views/process.rs`,
  `Row::absent`). Fixed. A line whose sources are neither refused nor
  in the run still says `not asked: needs ...`; a source the run did read,
  but that left nothing this line can draw on the target (results labelled
  `@system` only, which `on_target` drops, or no timebar bar from them),
  now says `nothing on the target from <source>`, dimmed, like `row.empty`.
  The machine gallery's Time line now reads `nothing on the target from
  cpu.oncpu_time` beside the CPU line drawn from that source. Shown by the
  new test `a_source_asked_on_the_machine_only_is_not_said_unasked` (a run
  holding only `cpu.oncpu_time@system`: CPU and Time say it, Memory still
  says not asked) and by `process--machine` re-blessed. The other
  `process--*` snapshots that still say `not asked` (system-wakeup,
  file-serving, vertical-abandoned) are runs that did not ask those
  sources, so nothing else changed.
- **Edits to `docs/view/gallery/**`, record's directory.** Acknowledged,
  not reverted: the snapshot tests compare against those files, so a view
  change without them is a red suite. This round touches one more pair,
  `process--machine.{txt,ansi}`, for the fix above. The frame conflict the
  reviewer foresaw did not happen: with views merged into `main`, `OMN_VIEW_BLESS=1
  cargo test --test view_snapshots` rewrites nothing in `frame--help` or
  `frame--narrow`, so main's copies are already what both streams draw.
  Still owed when **record** merges its new recordings: re-bless the
  gallery once after it (`OMN_VIEW_BLESS=1 cargo test -p omn-view` and
  `--test view_snapshots`), since its recordings redraw every `process--*`.
