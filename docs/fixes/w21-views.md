# The twenty-first wave, stream views

What the views asked of each other (`docs/dev/BRIEF.md`, "views"), on
`feat/w21-views`. Eight commits, all inside `view/**` (not `process.rs`),
`docs/view/*.md` (not `process.md`), `tests/view_model.rs`, and the gallery
snapshots my own changes redrew (see "Needs from another stream").

## What landed

- **The usage gradient** (`view/src/trust.rs`, `trust::usage`): one ramp of
  six 256-colour steps, green free to red full, by a share of a known
  capacity (`usage::colour`, `usage::of`, `usage::style`), and
  `usage::NEUTRAL` for a resource with none. `Trust::hued(colour)` keeps the
  hue whatever the trust and underlines a figure that must not be read at
  face value; `trust::figure_hued` puts the badges after it. The steps are
  `#00af00 #87af00 #d7af00 #ff8700 #ff5f00 #d70000`, the neutral `#5f87af`:
  none near white or near black, so a dark and a light terminal both read
  them. **`process` uses these**; nothing in it is mine to change.
- **`system` in the gradient**: the cpu (capacity: the machine), GPU busy
  (an engine's time), VRAM (the device's size), memory stalls (the wall
  time) per cell and per figure (`usage_chart`, `hue`); power, disk and
  network bytes and a target's RSS in the neutral hue, each section's rule
  saying why (`no capacity, so one hue`, `speed unknown, so one hue`).
  `docs/view/system.md`'s colour paragraph rewritten.
- **`system` asks `io.bytes` and `net.bytes` as `raw,timeline,summary`**,
  so read apart from written and in apart from out survive a finished run
  (the view already prefers events when a result holds them). README rows
  follow.
- **The mirror** (`view/src/model.rs`): `Results::fields`
  (`BTreeMap<String, Value>`, skipped when empty, as data's brief says),
  `Results::field_u64`, and a typed `Lifetime` with `adopted_ended`
  through `Results::launched()`. `Overhead::one_time_cpu_ns` was already
  there. `tests/view_model.rs` holds the `Lifetime` mirror to
  `omn::collect::Lifetime` field for field.
- **`request`'s `Undrawn::of` reads `fields`** (`never_ended`, `unborn`,
  `replaced`, `filtered_out`) and falls back on the sentences for a
  recording made before them; the names (`req:1 in task 7`) stay the
  notes'.
- **A ledger line of blocked time** that opens `timebar`: `sched.offcpu` as
  `summary` per task (with `fp` stacks, the request flame, timebar and lanes
  already make, so the merged needs stay one per source), its time off cpu,
  its rate as threads blocked at once, per task under it, `no block seen`
  for a target that never blocked.
- **Public readings** for `process`: `ledger::reading(data, focus) ->
  Vec<CostLine>` (resource, source, goto, and `Reading::Served { amount,
  value, blank, trust }`, `Refused` or `Absent`), and
  `flame::hottest(data, focus, off_cpu) -> Option<Hottest>` (name, share,
  own, total, trust). Documented in `docs/view/ledger.md` and
  `docs/view/flame.md`.
- **The timebar splits `sched.futex` out of the sleep**: a task's futex
  waits are taken out of its sleeping blocks, never more than the sleep, and
  drawn as `blocked: sleeping, on a futex` (a lock) beside the rest of the
  sleep, instead of naming the whole sleep a lock when the waits covered
  half of it and nothing otherwise. This is what made `process` read
  `sleep` on a workload that holds a lock 0.25 s.
- **The heatmap places drops at their drains** (`Results::drains`): `▓` in
  the drain's column, the stop's drain (a moment past the end) in the last
  column, a drain outside a zoomed window not drawn, and only what no drain
  placed said as `lost, when unknown`.
- **Flows' file -> device edge** from `io.fs`'s `blk_devs` matched against
  `io.bytes`' keys: a device a target's file sits above says `◂ N files`,
  and the file's detail says `above block device 259:0`. In words beside the
  junction, not yet a drawn ribbon.

## Shown versus reasoned

**Shown** (tests, on the recordings):

- `usage_runs_from_green_to_red_by_the_share_of_capacity`,
  `a_hued_figure_keeps_its_hue_under_a_loss` (trust.rs);
  `usage_is_coloured_by_how_full_and_bytes_keep_one_hue` (system, on
  `machine`: every cpu chart cell in the ramp, every network cell neutral).
- `requests_in_no_event_are_read_off_the_published_fields` (request);
  `the_lifetime_mirror_keeps_every_field` (view_model).
- `the_blocked_line_sums_the_time_off_cpu_and_opens_the_timebar` (ledger, on
  `workload-timebar`: the line is the events' `total_ns` sum to the
  nanosecond; the reading has every line in the sheet's order).
- `the_hottest_function_is_read_as_the_header_says_it` (flame:
  `fdatasync` > 90 % off cpu on `sqlite-commits`; the on-cpu name is on the
  drawn screen).
- `futex_waits_are_split_out_of_the_sleep` (timebar: a quarter of the sleep
  on a futex is a quarter as a lock, the parts sum to the sleep, more futex
  than sleep leaves no sleep).
- `a_drop_is_shaded_in_the_column_of_its_drain` (heatmap).
- `a_file_is_joined_to_the_device_under_it` (flows, on `file-serving`: one
  device fed, `dev:259+0`).
- On `workload-timebar` the worker's sleep now draws 0.477 s for the
  manifest's 0.40 s (in its 0.97x to 1.3x band), where it drew sleep plus
  lock before; see the request to record below.

**Reasoned, not shown**:

- How the ramp reads on a light terminal and in a 256-colour tmux: chosen
  for it, not looked at. Worth one look by the user on `omn top --gallery`.
- No recording holds `drains` yet, so no screen shows a placed drop; only
  the unit test does.
- The futex wait includes its wake-to-run, so the lock is clamped to the
  sleep and may slightly overstate the lock on a busy core.
- The live ledger now names more tasks (`frame--live`: "task 1493224 and 2
  more" where it named Thread-1): `sched.offcpu`'s live events are keyed per
  task and see the whole tree. More truthful, but it changes the title.

## Not done, and why

- **A drawn file -> device ribbon** in flows: the textual edge landed; a
  ribbon needs the junction's layout reworked. Next step.
- **The socket -> interface edge**: `net.socket` does not carry `iface` yet
  (collectors' field).
- **`--against` and focus snapshots in the standard set**: they live in
  `tests/view_snapshots.rs`, record's; `snapshot::draw_with` already serves
  them. Filed below.
- **`sqlite-latency`'s `blocked` with its events, `cpu.util` and `gpu.mem`
  as `raw`, the ledger's `cpu.oncpu_time` raw**: not asked this wave; only
  `io.bytes` and `net.bytes` moved.
- **A capacity for disks and links**: no source gives a device's or a
  link's speed, so they stay neutral; a link's speed from
  `/sys/class/net/*/speed` would be a collector's field.

## Privileged runs wanted

None of mine needs one to be judged; the views read recordings. To see the
new needs on a real run once merged and built (`make` first, for the
capabilities):

```console
make
target/debug/omn top --view ledger -- python3 view/workloads/timebar/workload.py   # the blocked line, enter -> timebar
target/debug/omn top --view system                                                  # the gradient, disk and network split live
```

## Decision lines

- The usage gradient is six even steps of the share of a known capacity,
  in the 256-colour cube; no capacity means one neutral hue and a sentence,
  never a guessed bound.
- A hued figure under a trust mark is underlined, not dimmed, and keeps its
  badge.
- A futex wait is taken out of the sleep, up to the sleep, rather than
  renaming the sleep by a half-coverage rule.

## Needs from another stream

- **record**: `tests/view_truth.rs`'s
  `timebar_splits_the_workers_wall_clock_as_its_phases` asserted the old
  rule; landed here in the fix round (see "Review").
- **record**: I re-blessed the gallery files my own changes redrew
  (`OMN_VIEW_BLESS=1` on `tests/view_snapshots.rs` and the ledger and
  timebar gallery tests), so the merge is green; the re-recording replaces
  them. Add `--against` and focus snapshots to the standard set through
  `snapshot::draw_with`.
- **process**: the `process` gallery files were re-blessed here in the fix
  round (see "Review"). Use `trust::usage` and
  `Trust::hued` for the lines, `ledger::reading` and `flame::hottest` for
  the figures, and the ledger's `blocked time` line for the Time line.
- **data**: `Results::fields` is mirrored as a map of name to JSON value;
  if `omn` writes another shape, `tests/view_model.rs`'s mirror test will
  say so.
- **collectors**: `net.socket`'s `iface` for the socket -> interface edge;
  a link's speed would give the network row a capacity.

## Review

The reviewer was right: on main after both merges, `view_truth`'s timebar
test, `views::process::tests::snapshot_on_the_process_workload`, and
`view_snapshots`' `process` and `frame_help` were red, all four from the
futex split. Rather than leave them to the record and process requests, the
fix round lands them on this branch, so merging it turns main green on its
own:

- `tests/view_truth.rs` (record's file): the worker's `░ blocked:
  sleeping` is held to `phase("sleeping")` and the new `blocked: sleeping,
  on a futex` line to `phase("futex")`, each 0.97x to 1.3x; the futex(2)
  line keeps its check. Passes on `workload-timebar`.
- `docs/view/gallery/process--*` and `frame--help` re-blessed
  (`OMN_VIEW_BLESS=1`) after merging main, so they are the process stream's
  merged view drawn with the split: a `lock` share on the Time line (10.5 %
  on `process-run`, 10.3 % on `workload-request`, 1.00 % on
  `many-requests`) and the sleep share less it. Only the Time line and the
  suspect's share changed in each.

After the fix round: `cargo test --test view_truth` 10/10,
`--test view_snapshots` 17/17, `-p omn-view --lib process` 11/11; clippy
and fmt clean.

## The suite

`cargo test -p omn-view --lib`: 193 of 194, the one failure `process`'s
snapshot (above). `tests/view_model.rs` 7/7, `view_live.rs` 5/5,
`view_timebar.rs` 1/1. `tests/view_snapshots.rs` 16/17 (`process`),
`tests/view_truth.rs` 9/10 (the timebar test above). `cargo clippy
--workspace --all-targets -- -D warnings` clean, `cargo fmt --check` clean.
