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
  earlier read until the next one, for a read and a half at most (review
  round: a gap of missing reads is blank). A measured zero is a dot on the
  baseline; a cell where nothing was measured is blank. A finished run's
  timelines are placed on the run's axis by inference (see "Review").
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
  joules spent (per domain from live events only: see "Review"); bytes per
  device and per interface, busiest first, on one
  scale, with the idle ones folded into one line (`i` lists them).
- **Live runs are drawn from their events**, through the same series code
  (one table of each collector's value attribute). The events also give the
  cpu by state (a stacked bar), read apart from written, in apart from out,
  the GPU's memory size and a process's `comm`. Where a recording kept no
  events, the section's rule says what is missing.
- **The process list**: one row per process and per thread any per-task
  source saw (a process's main thread its own row, `t<tid>`, since the
  review round)
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

## 5. Decision lines for `docs/DECISIONS.md`

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

- **core** (`src/emit.rs`, `view/src/model.rs`), raised by the review round:
  a timeline's offsets are measured from its result's first event
  (`first_ts_ns` in `aggregate`), and `results.json` does not say when that
  was. The system view now infers it for the machine's pollers (see
  "Review"), but the inference rests on the runner's polling discipline, and
  every view that lines up two timelines on one axis has the same problem.
  Either measure `offset_ns` from `window.run_opened_ns`, or write the
  result's `first_ts_ns` beside its timeline; `Results` in the view's model
  would read it and `timeline_origin` in `system.rs` would use it when
  present. The second keeps every existing `results.json` meaning what it
  meant.

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
  the RAPL domains by name (since the review round the view splits them, for
  both backends) and the process's `comm`, and the end of the run then
  drops them: a finished run's one `power.energy` timeline holds every
  domain together. Accepting `raw` (`emits:` in `catalog/{net,power,gpu,cpu}.yaml`)
  would let a finished run keep them.
- **core** (`view/src/app.rs`, `draw_header`): when the origin is cut, the
  run's length runs into the cost corner with no cell between them
  (`… s.yml) │ 1.64sview: snapshot`, seen replaying a recording with a long
  path). One cell of margin before the corner would fix it.

## Review

One round, five findings. Each is fixed below, with the one part of a
finding I disagree with said where it applies.

1. **"One time axis" did not hold on replay** (defect). Confirmed: on every
   fixture that measured the machine, `mem.pressure`, `io.bytes`, `net.bytes` and `power.energy` end
   about 252 ms before the run's close and `cpu.util`, `gpu.engine_busy` and
   `gpu.mem` 2-9 ms before it, all with the same `closed_ns`. Their
   collectors keep their first read as a baseline (`mem/pressure.rs`,
   `power/energy*.rs`), and a timeline's offsets start at its own first
   event. `results.json` does not say when that was, so the view now infers
   it: a poller's first round is at the window's opening and its last lands
   at the close (`the_runner_closes_the_window`, `src/collect/mod.rs`), so
   the first event is at the opening or one read later, and the view takes
   the one that puts the last read at the close (a quarter of a read of
   slack; the two answers are a whole read apart). After it, every machine
   row's last read is within 10 ms of the cpu's on the five fixtures that
   measured the machine
   (`every_timeline_of_the_machine_ends_where_the_run_does`). The target's
   timelines are left at the opening, and there I think the finding is
   wrong about the process sparkline: `cpu.oncpu_time`'s `fs:proc` reader
   takes its baseline in `start()` (`oncpu.rs:233`) and reports its first
   poll, and its last offset, 2.743 s on `http-server`, plus one read would
   be 2.993 s, after the target exited (2.839 s). So it is already in place.
   Live runs were already exact (event timestamps). The request to core for
   a recorded origin is in section 6.
2. **"Watts per RAPL domain" was not done** (defect). Half right. The perf
   unit's backend (`energy_pmu.rs`, the one the resolver prefers and the
   fixtures used) does emit a `domain` attribute, one event per domain, so
   live runs on it were split. The sysfs backend emits one event with
   `domains` and `domain.<name>`, which the view summed. And on replay the
   label `package` was a guess. Now: sysfs events are split per domain by
   their `domains` list; perf-unit events are keyed by domain and, past the
   first, package (two packages' `package` were one series); a finished
   run's one timeline is `all` when it has one point per read, one lane per
   domain by order (`#1`, `#2`, named "in the events only") when it has
   several per instant, and not drawn, with the reason, when the count
   differs between instants. Tests: `power_is_split_by_domain_from_the_events`,
   `a_timeline_of_several_domains_is_split_by_their_order_or_not_drawn`. The
   request to the catalog owners stands, now founded: by name, per domain is
   only in the events, and `power.energy` refuses `raw`.
3. **The box was cut silently on a short terminal** (gap). Fixed: when the
   box does not fit, whole sections are left out from the last, and one
   line in the loss colour counts and names them; the head, the axis line
   and the bottom border stay. `frame--narrow` (60x20) now shows
   `+5 left out, the terminal is too short: gpu mem…`. Test:
   `a_short_terminal_says_which_sections_it_left_out`.
4. **`served()` could draw a target's result as the machine's** (nit).
   Fixed in `system.rs`, not in `model.rs` (core's): without a
   `<source>@system` label, a section takes only results whose series name
   no task, and its rule says "scope not recorded: taken as the machine's"
   (a cgroup's `cpu.util` would look the same, and nothing in the result
   says). The target's RSS row reads the target's results directly. Test:
   `a_task_s_result_is_not_drawn_as_the_machine_s`.
5. **Smaller issues** (nits), all fixed: a finished run says `last`, a live
   one `now`; several engine or device tracks are labelled by their key; a
   process's row and its main thread's are two rows (`t<tid>` for the
   thread) instead of a process's cpu next to one thread's faults; a value
   is held across a read and a half, so lost reads are blank; `s` skips the
   sorts whose column the width hid. Tests:
   `the_target_is_in_the_list_with_its_cpu`,
   `cells_do_not_hold_a_value_across_missing_reads`,
   `sorting_skips_a_column_the_width_hid`,
   `a_finished_run_says_last_and_a_live_one_now`.

Snapshots re-blessed after being read: the seven `system--*`, plus
`frame--help` and `frame--narrow` (first tab). The visible changes: delta
rows start one read later on the short fixtures, `last`, `all`, the
`t<tid>` rows, one column more for the id, and the narrow frame's
left-out line.


### The suite, after the review round

`/tmp` was at its quota (about 2400 leaked `/tmp/omn-*` test directories,
24 GB, none of them this stream's; I did not remove them), so every run
below had `TMPDIR` pointed at the worktree's `target/tmp`. One `make check`
was red at `cases::every_case_names_its_known_cause_both_ways`
(`make-sqlite`, a load-dependent attribution sentence). One
`cargo test --workspace --no-fail-fast` then gave 2378 passed, 7 failed,
11 ignored, with no failure in `view/` (that test passed this time). After
`make` regranted the capabilities, each of the 7 was rerun alone once: six
passed (`the_runner_closes_the_window`, the capability refusals, `unwind`'s
two, the `omn-mcp` walk-through), and
`runner::a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed`
stays red, the open gap already listed in `docs/open-findings.md`. Clippy
and fmt were clean.
