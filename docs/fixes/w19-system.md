# The nineteenth wave — stream system

Owns `view/src/views/system.rs`, its snapshots (`docs/view/gallery/system--*`)
and `docs/view/system.md`. Two snapshots outside the row changed because
they draw the first tab, which is this view: `frame--help` and
`frame--narrow` (the frame's code and tests are untouched; the snapshots
were blessed after being looked at).

## 1. What landed

- **The overview, as one box on one time axis.** A section per resource
  (cpu, GPU, memory, power, disk, network), each chart in the same columns
  and spanning the same run, so a column is one moment from top to bottom.
  The axis says what a cell is worth ("a cell is 54.8ms, drawn at its
  peak"). A cell holds the largest value that fell in it and carries the
  earlier read until the next one. A measured zero is a dot on the baseline;
  a cell where nothing was measured is blank.
- **Colour is the trust style and nothing else.** Charts are drawn in the
  terminal's colour; a section's badges sit on its rule and dim its charts and
  figures. `gpu.engine_busy`'s `ts_err_ns` (1 s) is drawn as a cyan ruler at
  its real width on the run's axis.
- **Figures**: cpu now/mean/peak against a round ceiling, plus "N cpus busy,
  on average" from the cpu result's own `observed_cpu_ns / wall_ns`; GPU
  occupancy mean/peak and memory held (against the device's size when the
  events carry it, otherwise "held the whole run, unmoved" rather than a
  full bar); memory stall (`never stalled`, `<0.01%`) and the target's RSS;
  watts per RAPL domain from joules over each read's interval, plus the
  joules spent; bytes per device and per interface, busiest first, on one
  scale, with the idle ones folded into one line (`i` lists them).
- **Live runs are drawn from their events**, through the same series code
  (one table of each collector's value attribute). The events also give the
  cpu by state (a stacked bar), read apart from written, in apart from out,
  the GPU's memory size and a process's `comm`. Where a recording kept no
  events, the section's rule says what is missing.
- **The process list**: one row per task any per-task source saw
  (`cpu.oncpu_time`, `mem.rss`, `mem.faults`, `sched.runqlat`, `io.syscall`,
  `sched.futex`, `gpu.mem` on the target). The columns are as many as fit:
  cpu share of one cpu with its chart, cpu time, RSS, faults, run queue,
  I/O calls, time in I/O, on locks, GPU memory. A cell nothing measured is
  `·`, not `0`. Each column's badges are said once under the list. `↑`/`↓`
  select, `s` sorts, `enter` opens the ledger narrowed to a pid (a process
  row) or a tid (a thread row).
- **The cost corner**: what each of the view's collectors cost as a share
  of one cpu, with bars and a total. `≥` and dimmed where an in-target cost
  is unknown. When too many collectors fit, the rest are folded into one
  line and still counted in the total.
- **Refusals and absences**: a refused source is drawn as its reason in its
  place (the `machine` fixture's process list). A run that measured nothing
  of the machine gets one paragraph naming what its session did ask for.
- Tests: 11 unit tests in `system.rs`. They cover every section on the
  `machine` fixture, the ruler's width, the target's cpu share
  (0.8-1.1 on `cpython-hot`), every thread of `http-server` listed with `·`
  where unmeasured, the no-machine paragraph, `enter` giving a pid and then
  a tid, sorting, a synthetic live run drawn from events with its splits and
  name, cell resampling, per-second conversion, and round ceilings. Plus
  `cargo test --test view_snapshots system` on the seven fixtures.

## 2. Shown, and reasoned

Shown:
- The seven fixture snapshots (`docs/view/gallery/system--*.txt` and `.ansi`)
  at 120x40, and the frame's narrow snapshot at 60x20.
- **The events path on a real recording**, not only in a unit test.
  `omn top --dump-session -- work.sh`, with `raw` added to `cpu.util`,
  `io.bytes` and `gpu.mem`, was recorded with
  `omn top -f s.yml --headless -o DIR` (unprivileged binary) and replayed.
  It showed the by-state bar (user 70 %, sys 19 %, iowait 11 %), read apart
  from written per device, and "668MiB held of 11.0GiB". The screen is in
  `docs/view/system.md`. The recording itself is not committed: fixtures
  are core's.
- **That the catalog refuses `raw` for `net.bytes`, `gpu.engine_busy`,
  `power.energy` and `cpu.oncpu_time`** ("cannot emit raw"). It was shown by
  asking for it in that session.
- That the machine sections are the machine's: in that recording, 1.25 GiB
  read and 2.5 GiB on `lo` were not the target's (it wrote to a tmpfs,
  `findmnt` says so), and the launched shell's `cpu.oncpu_time` was 0 ns
  while its children did the work.

Reasoned, not shown:
- That a live run in a terminal looks like the snapshots. The live path is
  drawn from events by the same code, and the unit test exercises it on
  synthetic events. No live run was looked at on a real terminal: the agent
  shell has none (`omn top` refuses there, by design).
- That "N cpus busy" covers the same window as the chart. Both come from
  the same result: the figure from its overhead account, the chart from its
  timeline. Against the chart's mean times this machine's 24 cpus they
  differ by 1-3 % on `machine`, `cpython-hot` and `sqlite-commits`, and by
  12 % on the 2.8 s `http-server` run. The account's window and the polls'
  are not the same edges, and a short run feels that most. The figure is
  labelled "on average" and not reconciled further.

## 3. What I could not do, and why

- **Keep the splits after a run ends.** In/out, names and by-state live in
  events only. The needs do not ask for `raw`, because the needs table in
  `docs/view/README.md` is core's and `tests/view_model.rs` holds it to the
  code, and four of the sources refuse `raw` in the catalog. Both are below
  as requests.
- **Commit a snapshot that shows the splits.** The fixtures carry no `raw`
  for these sources, and re-recording them is core's (`record.sh`).
- **Name devices** (`nvme0n1` rather than `259:0`): the series key carries
  only `major:minor`, and a view reads no file.

### The suite

One `make check` before the last commit was red at `budget` (flaky tally 14,
which passed alone). One `cargo test --workspace --no-fail-fast` then showed
one failure of mine: `view_live::a_gallery_prints_every_view` wants every view
to show `┌ <name> -- ` or its title. The box now opens with
`┌ system -- <title>`, and the snapshots were re-blessed. After that fix, a
second no-fail-fast run gave 2363 passed, 14 failed, 11 ignored, with clippy
and fmt clean. Every failure was outside `view/`. Eleven came from the binary
missing its capabilities after `cargo` relinked it (the "exits three"
refusals, `unwind`). After `make` regranted them, each was rerun alone once
and passed, as did `budget`. Two stay red when run alone:
`runner::a_profile_s_losses_land_...`, the open gap in
`docs/open-findings.md` (not load), and
`omn-mcp walkthrough::the_walk_through_hands_over_...`, where
`cpu.profile` went over its 25 % budget on a loaded machine (1.79 s of
startup). Neither touches this stream's files. The first runs' logs were
also unreliable: the scratchpad is shared, and another stream's `nff.log`
landed in the same file (the finding "How the wave ran" already names). The
later runs logged to a private subdirectory.

## 4. Privileged runs wanted

None for this view: it reads only what a run holds. One interactive look by
the user, on a real terminal, after `make` (which grants the capabilities):

```console
omn top -- python3 cases/cpython-interpreter/hot.py   # live, then q
omn top --gallery tests/view_fixtures/sqlite-commits   # space steps the views
```

## 5. Decision lines for `DECISIONS.md`

- **The overview draws every resource on one time axis**: one box, a
  section per resource, every chart in the same columns over the same run,
  each cell the peak of what fell in it. *Why*: side-by-side panels scaled
  to their own widths cannot line up a disk burst with the cpu that caused
  it, and alignment is what an overview of one run is for.
- **On the overview, colour is trust only**: charts in the terminal's
  colour, so every coloured cell is a loss, a time error, a refusal or a
  cost. *Why*: a screen where most sources are `low` determinism must make
  the exceptions stand out, not compete with decorative colour.
- **A cell measured at zero and a cell not measured are drawn apart** (a
  dot, a blank), and a table cell nothing measured is `·`, never `0`.
  *Why*: an empty series and a series of zeros say different things about a
  run, the same rule `gpu.mem` follows.

## 6. Needs from another stream

- **core** (`docs/view/README.md`, `tests/view_fixtures/record.sh`): to keep
  the cpu by state, read/written and the GPU's memory size after a run ends,
  `system.rs`'s needs would add `raw` to `cpu.util`, `io.bytes` and `gpu.mem`.
  The README rows change to `raw,timeline,summary` in the same commit, then
  the fixtures are re-recorded and the snapshots blessed. Estimated size: about
  20 events/s (one cpu, three devices, one GPU at 4 Hz), roughly 16 kB/s of
  JSON, about 10 MB for a 10-minute run. I made no change to `NEEDS`, so the
  table stays true. The coordinator decides whether the size is worth it.
- **catalog owners of `net.bytes`, `power.energy`, `gpu.engine_busy`,
  `cpu.oncpu_time`**: these refuse `raw`. In a live run they carry rx/tx,
  the RAPL domain and the process's `comm`, and the end of the run then
  drops them. Accepting `raw` (`emits:` in `catalog/{net,power,gpu,cpu}.yaml`)
  would let a finished run keep them.
- **core** (`view/src/app.rs`, `draw_header`): when the origin is cut, the
  run's length runs into the cost corner with no cell between them
  (`… s.yml) │ 1.64sview: snapshot`, seen replaying a recording with a long
  path). One cell of margin before the corner would fix it.
