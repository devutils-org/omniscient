# The twenty-third wave, stream power

The power family's seven pairs, rewritten to the runner's schema and run by
it. Branch `feat/w23-power`: `dadf8fb` (the pairs and the table) and this
handoff. Files touched: `catalog/references/power.yaml`,
`docs/replaces/power.md`. `tests/reference/power.rs` and the captures are
unchanged: every compare is a path, so no `omn_parse` parser was needed, and
no tool that runs unprivileged here had a capture written from
documentation.

## What landed

- **Every pair is `run: alongside`**, not `tool_wraps_omn` as the brief
  pencilled in. omn still refuses to launch a program for a system- or
  cpu-scoped source (`--system` cannot be used with `-- PROGRAM`; without
  it, "cannot be measured per launch"), so under `tool_wraps_omn` there is
  no workload and nothing for the load the energy and frequency pairs need.
  `alongside` gives both: omn over `-d 2s`, the tool wrapping `sleep 2` (the
  common use, `perf stat -a -- sleep 2`, `turbostat ... sleep 2`,
  `cpupower monitor ... sleep 2`), started together, then the runner
  launches `spin.py` after `settle_ms: 100`. Both windows are 2 s, offset
  by omn's start-up, instead of the tool's window holding omn's start-up on
  top of omn's (the 2.3 % bias the twenty-second wave carried).
- `background: true`, `same_run`, `omn_series` and `summary.mean` are gone;
  each compare is a path, `results.0.outputs[emit=summary].series[key=...].mean`
  (or `.last`). `privileged: false` dropped where it was the default.
- Each pair has an `id`; `docs/replaces/power.md` names it in its row, with
  the runner's numbers.
- **`freq-turbostat` reads omn's `last`, not `mean`**: today's
  `power.freq -m poll@2s -d 2s` gives two readings (window open and close),
  and the first comes before the load starts, at the idle cpu's 1745.6 MHz.
  `cpuinfo_avg_freq` is the last scheduler tick's APERF/MPERF, so under a
  load held across the window the closing reading is the plateau turbostat's
  Bzy_MHz averages. Said in the pair's `why`.

## Each pair

| Pair | Outcome | Three runs, omn / tool |
|---|---|---|
| `energy-perf-stat` | holds (rel 0.05) | 238.04 / 237.16 J, 210.13 / 210.03, 223.93 / 223.09 |
| `energy-turbostat` | holds (rel 0.05) | 228.28 / 228.31 J, 200.94 / 200.26, 209.52 / 210.01 |
| `energy-rapl-files` | skipped: `sudo -n` does not grant root (`energy_uj` is root-only) | -- |
| `freq-turbostat` | holds (rel 0.03) | 3924 / 3984 MHz, 3974 / 3977, 3938 / 3975 |
| `freq-cpupower-mperf` | skipped: `sudo -n` (Mperf reads `/dev/cpu/*/msr`) | -- |
| `cstate-cpupower-idle-stats` | holds (abs 0.1 s) | cpu 1: 0.976 / 0.986 s, 1.193 / 1.219, 1.350 / 1.342 |
| `temp-sensors` | holds (abs 3) | 80.875 / 80.875, 79.25 / 79.25, 79.625 / 79.75 |

Tally, three times: `5 held, 2 skipped, 0 gaps, 0 known defects, 0 failed`.

## Shown vs reasoned

- **Shown**: `make`, then `cargo test --test reference pairs_power --
  --nocapture` three times, under the other streams' load, every run the
  tally above; `cargo test --test reference power::` (9 parser tests),
  `families_are_whole`; `every_pair_is_well_formed` and
  `every_row_names_a_pair_or_why_none` name no power pair or row (both are
  still red on other families' rows, theirs to rewrite);
  `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo fmt --check` clean.
- **Shown**: `powertop` is installed now and refuses to run unprivileged
  ("must be run with root privileges"); its row stays `no pair`, with that
  reason.
- **Reasoned, not shown**: the two privileged pairs (no sudo); the
  `requires_setting` skip above `perf_event_paranoid` 0 (this machine is at
  -1). `cpupower_mperf.txt` and the two `rapl_window*.txt` captures are
  still written by hand in the tools' format, since both need root.

## Not done, and why

- No `tool_wraps_omn` pair: nothing for it to add here once `alongside`
  holds the same window without the start-up bias, and the runner's caveat
  (perf events inherited past the capable `omn`'s exec count nothing) would
  make `perf stat -a`'s wrapping fragile to reason about even though `-a`
  does not inherit.
- No pair for `power.throttle`, `powertop`, `cpupower -m RAPL` or
  turbostat's other columns: the reasons are in the table, unchanged from the
  twenty-second wave except powertop (installed, root-only).
- The omn gaps of `docs/fixes/w22-power.md` stand (program-bounded system
  window, core energy domain, `Busy%`/`Avg_MHz`, `power.cstate -e rate`,
  per-process wakeups and energy): the next waves'.

## Privileged runs wanted

```console
sudo -v   # by a person, once
cd /home/moha/repos/omniscient && make && cargo test --test reference pairs_power -- --nocapture
# energy-rapl-files and freq-cpupower-mperf then run; Mperf also needs the msr module,
# which this wave does not load: `sudo modprobe msr` first, by a person.
```

Then replace `captures/power/cpupower_mperf.txt` and `rapl_window*.txt` with
the real outputs.

## Decision lines

- Power pairs run `alongside`, the tool wrapping `sleep 2` and omn on
  `-d 2s`, with the load launched by the runner after a 100 ms settle: one
  load both windows see, two windows of equal length.
- A cpu-frequency pair under a held load reads omn's `last`: the reading at
  window open precedes the load.

## Needs from another stream

- **Coordinator**: `docs/replaces.md`'s power line, from the table.
- **src (later)**: `power.freq`'s poll at window open (before anything the
  user launched alongside it); and the twenty-second wave's gap 1, a
  system-scoped window bounded by a launched program, which would let these
  pairs be `tool_wraps_omn` over the program itself.
