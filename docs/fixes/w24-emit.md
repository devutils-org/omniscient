# Wave 24, stream emit -- the results layer

Branch `feat/w24-emit`. Defects first, then features; every item below
was run on this machine.

**Shared layer, said first:** one small commit `fix(catalog): a backend's own
emits, a non-empty subset of the entry's` (`68fbdf8`) in `src/catalog.rs`.
heap 1 is a defect (an invalid combination refused after the run), and the
chain had no way to say that one backend serves fewer emits than its
siblings: modes, keys, targets, filter fields and units each had a
per-backend list, emits did not. The commit adds `Backend::emits`,
`Entry::backend_emits` and a loader check (no empty list, nothing wider than
the entry, every entry emit produced by some backend), with its unit test.

`src/encode.rs` is in no row of the table. It is the results layer's text
and JSON encoder, and the summary's `total` and the zero ranking needed one
line each there, so this stream took it as part of its row; nothing else in
it changed.

## What landed

| Commit | What |
|---|---|
| `68fbdf8` | `fix(catalog)`: per-backend `emits:` (above). |
| `0ec5cd2` | `fix(resolve)`: a backend that does not produce an emit the request asks is passed over with the reason, walked or pinned, so the refusal comes before the launch (heap 1). If no backend declares the request, `NoBackendDeclares` names the emits as well. |
| `baa6ff3` | `feat(emit)`: `summary` carries `total` (heap 4, top 1, net); `summary@<attr>` and `hist[:<scale>]@<attr>` (gpu 2); a zero ranking row and no `-0.0` (perfsched 5); a regrouped poll adds up the readings of one tick (io 5). |
| `7552e22` | `feat(emit)`: the `func` key, the leaf frame of each event's stack (perfrecord 3). The text says when a ranking counted nothing. |
| `79ab48f` | `fix(resolve)`: `topk:N@<attr>`, `summary@<attr>`, `hist@<attr>` over a field the resolved backend never fills is refused before the run, the same way a filter on it is (perfrecord 1). |

Details:

- **perfrecord 1** (filter on a field the backend never fills). The
  resolver already refused it from the backend's `filter_fields`
  (`check_filter_fields`). What was missing is the data: `cpu.profile`'s
  `pmu:sw` inherits the entry's `[comm, cpu]` and its collector fills no
  `comm`. The line is the cpu stream's (requests). The `@<attr>` emits now go
  through the same check.
- **heap 1**: the resolver passes over a backend whose own `emits` leave out
  an emit the request asks for. An emit the *entry* does not offer is still
  the validator's to refuse, in its own words. The data line is the mem
  stream's (requests).
- **`summary.total`**: `count x mean` exactly, written where the source's
  values add up: its entry declares `rate`, `topk` or `stack`
  (`emit::values_add_up`). It is absent over a level (`mem.rss`,
  `mem.heap.live`, `gpu.mem`, ratios, temperatures), where a sum of readings
  means nothing. It is marked `>=` as `n` is when the run lost records. The
  text prints `total X` at the end of the summary line.
- **`summary@<attr>`, `hist@<attr>`, `hist:log2@<attr>`,
  `hist:linear:<w>@<attr>`**: `Emit::SummaryBy` and `Emit::HistBy` are new
  variants. `Emit::Summary` and `Emit::Hist` are unchanged, so no other file
  had to change. Rows are named `<attr>=<value>`, as `topk:N@<attr>`'s are,
  and `<attr>=?` holds events that carry none, with a degradation saying how
  many. The validator checks the attribute against the entry's
  `filter_fields`, as it does for the ranking. `Emit::by()` returns the
  attribute for all three.
- **The zero ranking**: when a ranking counted nothing over a window it did
  measure, it holds one row named by the target (`pid:N`, `tid:N`, `launch`,
  `system`, ...) at 0 events and 0 total. The text says "nothing was counted
  over the window". If the run measured no window, the ranking stays empty.
  `rest.total` is a fold from `0.0`, because `f64`'s own `sum` of nothing is
  `-0.0`.
- **io 5**: if a request is a poll keyed on fewer keys than its events
  carry, `summary` and `hist` first add the readings that share a timestamp
  within a series into one. A traced event is never folded. The `@<attr>`
  forms are not folded either.
- **`func` key** (`KeyKind::Func`, appended at the seam): no collector
  attaches it. `emit::aggregate` puts the leaf frame of each event's stack on
  the event as the `func` attribute, from the collector's stack table or from
  the frames the event carries. `SeriesKey::by` reads a derived key
  (`KeyKind::derived`) from that attribute. An unnamed frame is
  `object+0xoff`, one row per address. An event with no stack is `func:?`. A
  source offers the key by listing `func` in its `keys:` (requests).

## Shown vs reasoned

Shown on this machine. Where a family YAML line is still another stream's,
the run used a scratch copy of `catalog/` through `OMN_CATALOG_DIR` that has
only that line added.

- **perfrecord 1**: `omn trace cpu.profile -m sample@997hz -e topk --filter
  'comm == "sh"' -d 300ms --system`. Before: it ran 1.37 s and printed "the
  0 heaviest of 0 series". With `pmu:sw`'s `filter_fields: [cpu]`: refused
  at once, exit 3, "`cpu.profile` reads `comm`, in its filter or an `@comm`
  emit, which the selected backend `pmu:sw` does not carry: `pmu` does, and
  this build has no collector for it yet". `-e topk:5@comm` is refused the
  same way.
- **heap 1**: `omn trace mem.heap.alloc -e stack --format folded -- true`.
  Before: the program ran, then "`stack` was asked for and no event carried
  one". With the preload line: refused before the launch, "`preload` was
  passed over: `preload` emits `hist`, `topk`, `raw`, `summary`, not
  `stack`", and the chain ends at "no collector is written for
  `mem.heap.alloc` on backend `bpf:usdt` yet". `-e topk` on the same
  catalog still runs on `preload`.
- **gpu 2**: `omn trace gpu.kernel -e summary@kernel -e topk:20@kernel --
  python3 tests/gpu_workload.py launches 2000` gives `kernel=tiny n=2000 min
  1.22us mean 1.25us max 1.82us total 2.49ms`. The workload prints "2000
  launches of tiny", and the ranking's total for the same row is 2.49 ms.
- **heap 4 / top 1**: `mem.heap.alloc -e summary -e topk:1 -- python3 -c
  'x=[bytearray(1<<20) for _ in range(20)]'` gives summary `n=5095 ...
  total 23.4 MiB` against topk's `23.4 MiB over 5095 events`. 20 MiB of that
  is the workload's own 20 x 1 MiB.
- **perfsched 5**: `omn trace sched.migrations -m count -e topk --pid <a
  sleeping pid> -d 200ms`. Before: "the 0 heaviest of 0 series" and
  `rest.total: -0.0` in the JSON. After: `1. pid:N 0 count over 0 events`,
  "nothing was counted over the window", and `rest.total: 0.0`.
- **io 5**: `omn trace io.bytes --system -m poll@100ms -k dir -e summary -e
  raw -d 1s`, beside a 200 MiB `dd oflag=direct` write. `dir:write` has
  `count 10` (10 ticks, 2 devices; before it was 2 per tick), mean 2179072,
  max 7741440, total 21790720. A script over the run's own raw events (per
  tick, write bytes summed over devices) gives 10 ticks, mean 2179072, max
  7741440 and total 21790720. That script is independent of the folding
  code but not of the collector; `/proc/diskstats` totals are the io
  stream's pairs.
- **perfrecord 3**: on a C workload of 600M and 200M iterations
  (`heavy`/`light`, 3:1, `-fno-omit-frame-pointer`), `omn trace cpu.profile
  -m sample@997hz -k func -e topk:5` gives `func:heavy 1065`,
  `func:light 350` (75.27 % / 24.73 %). `sudo -n perf record -e cpu-clock
  -F 997` plus `perf report --sort sym` gives 74.78 % / 25.22 %.

Reasoned only: the per-backend `emits` loader rules beyond their unit test;
that `values_add_up`'s rule (entry declares `rate`, `topk` or `stack`) puts
every source on the right side. I read the classification for all 99
summary sources: `cpu.throttle`, `net.qdisc` and `power.cstate` fall on the
"level" side and so get no total. That is conservative: a total is
withheld, never wrong.

## Tests

- `cargo test --lib` for `emit`, `request`, `resolve`, `catalog`,
  `validate`, `encode` (and `filter`, `format`, `render`): all green. New
  tests: `a_backend_emits_list_is_a_non_empty_subset_the_chain_covers`
  (catalog);
  `a_backend_whose_emits_leave_the_requests_out_is_passed_over_with_the_reason`
  and
  `an_attribute_emit_over_a_field_the_backend_never_fills_is_refused`
  (resolve); `a_summary_totals_increments_and_not_levels`,
  `a_summary_and_a_histogram_per_attribute_value`,
  `a_ranking_of_nothing_counted_is_one_zero_row_and_no_negative_zero`,
  `a_regrouped_poll_folds_the_readings_of_one_tick`,
  `a_function_key_is_the_leaf_frame_of_the_stack` (emit); the `@<attr>`
  round trips (request).
- Integration tests that read the results layer: `aggregation` 17,
  `join_engine` 15, `encoders` 26 (+1 ignored), `cli` 28 (33 after merging main, green), `recorder` 8. In
  `recorder`, `two_bursts_write_exactly_two_documents_each_holding_its_burst`
  failed once under the wave's load and passed when rerun alone (a timing
  test; noted, not widened).
- `catalog_model::every_key_kind_parses_back_from_what_it_prints`: green.
- `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt
  --check`: clean.

## Not done, and why (ranked)

1. **An `object` key** (perfrecord 4). The stack table carries an object
   only for unnamed frames (`libc.so.6+0x27781`). A named frame is the bare
   function name, so an `object` key would be `object:?` for all named code,
   a misleading table. It needs the unwinder to keep the object of every
   frame (unwind stream, `src/stack.rs`).
2. **Inclusive per-function share** (perfrecord 3, `perf report`'s
   children). Each sample would count once for every distinct function on
   its stack. That is a different ranking, not a key, and was left for
   after the self share.
3. A summary over `sched.runqlat`'s `wake_to_run_ns` (perfsched 8) is in
   `STATE.md`'s list but not in this stream's section of the brief, so it
   was not started.
4. `omn list` does not show a backend's own `emits` (it shows no
   per-backend modes either).

## Needs from another stream

- **cpu** (`catalog/cpu.yaml`, `cpu.profile`): on `pmu:sw`, either fill
  `comm` on every sample or add `filter_fields: [cpu]` to the backend, so the
  resolver refuses a `comm` filter before the run (perfrecord 1). Add `func`
  to the entry's `keys:` (`keys: [task, cgroup, cpu, func]`) so `--keys
  func` validates (perfrecord 3).
- **mem** (`catalog/mem.yaml`, `mem.heap.alloc`): until the shim captures
  stacks, add `emits: [hist, topk, raw, summary]` to the `preload` backend
  (heap 1). The loader accepts that line as it is.
- **reference**: pairs that compute `count * mean` in `omn_jq`
  (`tests/reference/top.rs`, heap, net) can read `summary.total` now. It is
  present on sources whose values add up.
- **run** (`src/cli.rs`): the `--emit` help could name `summary@<attr>` and
  `hist[:<scale>]@<attr>` beside `topk:N@<attr>`.
- **unwind**: the object of every frame in the stack table, for an
  `object` key.
- **Families adding a key**: a key no collector attaches but that can be
  read off an event attribute can use `KeyKind::derived` (the event
  attribute named after the key). A key that needs only an appended variant
  needs nothing from this stream.

## Decision lines for `DECISIONS.md`

- A backend may declare its own `emits:`, a non-empty subset of the
  entry's. The resolver passes over one that does not produce an asked emit,
  with the reason, before the run (heap 1).
- `summary.total` is written only where the source's values add up, meaning
  its entry declares `rate`, `topk` or `stack`. Over a level it is absent
  rather than a meaningless sum.
- `summary` and `hist` take `@<attr>` as `topk` does. Rows are
  `<attr>=<value>` and are validated against the entry's `filter_fields`.
  The resolved backend must carry the attribute, as for a filter.
- A ranking that measured a window and counted nothing holds one zero row
  named by the target, not none.
- A poll regrouped by fewer keys than its events carry adds up one tick's
  readings into one measurement for `summary` and `hist`, but only where
  the source's values add up. A level keeps its readings apart.
- `func` is a derived key: the emit layer reads it from the leaf frame of
  each event's stack. No collector attaches it. `--keys func` with
  `--stack none` is refused, and the events that carried no stack are
  counted in a degradation note.

## Review

The fix round answered two findings. Both were right.

- **The tick fold summed levels (defect).** `groups_for` folded every
  regrouped poll, so `net.tcp.rtt -m poll -k task -e summary` over three
  connections reported the sum of three RTTs per tick. The fold is now
  gated on `polled && values_add_up(source)`. A level regrouped by fewer
  keys keeps one measurement per reading, which is what it did before the
  io 5 change. `a_regrouped_poll_folds_the_readings_of_one_tick` now runs
  the fold over an additive source (`net.packets`). It also checks that
  `mem.rss`, a level, keeps all five readings: count 5, mean 4, not 3 ticks.
- **`func:?` with no reason (gap).** `derive_func` now returns how many
  events had no stack it could resolve. When that number is above zero the
  result carries "N of M events carried no stack to name a function by, so
  `--keys func` holds them as `func:?`". `check_stack` refuses `--keys func`
  together with `--stack none`, with a hint. A unit test asserts that every
  embedded entry offering `func` also declares the `stack` emit. None offers
  it yet, so the test guards the cpu stream's addition. Making a collector
  capture stacks when only `-k func` is asked (mem.heap.alloc captures them
  for `-e stack` only) is collector code, outside this stream's files. Until
  a collector does it, the degradation note is how a stackless run shows up.

## Privileged runs wanted

None beyond what `sudo -n` granted (`perf record`/`perf report`, run above).
