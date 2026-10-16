# Stream run, the twenty-fifth wave

Branch `feat/w25-run`. Done sequentially by one agent, not in parallel.

## Outside the owned files, said first

- **`src/collect/mod.rs`** is in the row, but `Losses`/`Overrun` is the
  shared runner: `Overrun` gains `skipped` (`serde(default)`), merged in
  `absorb`.
- **`view/src/model.rs` and `view/src/views/process.rs`** (stream views'):
  `Beside::capacity` and `Data::capacity` (a `MachineCapacity` of cores and
  `MemTotal` bytes), `mem_total` moved to the model, and `Capacity::of`
  reading a recording's capacity. Done here end to end because the run's
  half (filling it) cannot land without the field; views' item "Beside::capacity"
  is therefore done.

## What landed

1. **A poll round that never ran is counted** (`Overrun::skipped`). A late
   read re-bases its schedule on now, so a round that held a 250 ms poll for
   three seconds was one late tick and eleven readings counted nowhere. The
   late-tick reason now ends with `; N more readings fell due meanwhile and
   never ran`.
2. **`omn latency` and `omn saturate` exit 1** when their answer carries
   `failures`, through `outcome`, which prints each failure and the "exit 1"
   line.
3. **`Tap::described` sends `Update::Described`** for each result whose
   value attribute, row stamp or drains changed once events were handed
   over; before that, the opening data again (`Update::Restart`), as before.
4. **`Beside::capacity` filled**: `omn top -o` writes the machine's cores and
   `MemTotal` in `view.json`, and a replay's process view reads them instead
   of drawing no gradient.
5. **The cpu spent after a window closed** is named in the cost line when it
   outweighs the window: `; and 69.0ms after it closed (detaching, resolving
   what it read), which is in no percentage either`.
6. **Help**: `--emit` names `topk:N@<attr>`, `summary@<attr>` and
   `hist[:<scale>]@<attr>`; `--program-stderr` names `FILE.1 ... FILE.N`
   under `omn latency --runs N`.

## Shown vs reasoned

- **The starvation, measured unprivileged** (shown, partly): a 3 s
  machine-wide `omn top --headless` on this machine (24 cpus) read
  `cpu.util` 12 times of 12 at 250 ms, `mem.pressure` 12, `proc.tasks` 1632
  events, no late tick. But this machine grants no capabilities, so none of
  the trace sources that starved the pollers (`sched.offcpu`'s stacks)
  attached: the starvation is not reproduced, and whether `8a72e51` /
  `f36d98c` (framehop modules mapped, the closing read) cured it here is
  not shown. The count added in item 1 is what will show it.
- **Capacity** (shown): `view.json` reads `{"cores": 24.0, "memory_bytes":
  67326771200.0}`; `nproc` says 24, `MemTotal` 65748800 kB = 67326771200 B.
- **The cost line** (shown by a unit test only): no source that unlinks
  after its window attaches without capabilities here.
- **Exit codes of latency/saturate** (reasoned): the same `outcome` the
  trace path uses and tests; no failing answer was produced on this machine.

## Privileged runs wanted

```console
make   # with the capabilities granted
./target/debug/omn top --headless -d 3s -o /tmp/top-priv
python3 -c "import json;[print(r['label'],r['events_seen'],r['losses']) for r in json.load(open('/tmp/top-priv/results.json'))['results']]"
```

`cpu.util@system` should read about 12 events; if not, its losses now say
how many readings never ran. And `omn trace io.fsync -- <fsync loop>` for
the after-window clause.

## Tests

`cargo test --bin omn` (11, two new or rewritten), `cargo test --lib
collect::tests` (55, one new), `cargo test -p omn-view` (209, one new),
`cargo test --test cli --test runner --test view_model`, clippy, fmt.

## Decision lines for `DECISIONS.md`

- A late round's swallowed readings are counted beside the late tick, not
  as late ticks: the tick count stays "rounds that overran".
- A recording carries its machine's capacity in `view.json`; a recording
  without one draws no gradient rather than the replaying machine's.
