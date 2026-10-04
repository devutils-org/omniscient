# The twenty-third wave, stream sysstat

The sysstat family's thirteen pairs, run by the shared runner. Branch
`feat/w23-sysstat`, on `main` at `926adc7` (stream runner merged).

## What landed

- `catalog/references/sysstat.yaml`: every pair rewritten to the README's
  schema, each with an `id`. The family's own fields are gone:
  `target: system` is `run: alongside` (`settle_ms: 200` where a spinner is
  the workload, so both pollers open their window before it starts);
  `compare.omn_key` and `summary.<stat>` / `rate.<field>` are dotted paths
  (`results.0.outputs[emit=summary].series[key=node:0].max`,
  `...series.0.max`, `results.0.outputs[emit=rate].series.0.per_second`);
  the `raw.<attr>` mean is `compare.omn_parse`. `known_defect` on
  `pidstat-w` is as written before.
- `tests/reference/sysstat.rs`: three `omn` parsers, `omn_cpu_util`,
  `omn_mem_swap`, `omn_psi`, all built on `raw_means` (each numeric raw
  attribute's mean, a leading event stamped within half a period of the
  window's opening left out: defect 2 below), renamed to the tool's keys
  (`busy`/`all`; `si_bytes`/`so_bytes`; `some`/`full` from either
  `cpu.pressure`'s `some` or the siblings' `stall_ratio` /
  `full_stall_ratio`). Each unit-tested on a capture of `omn`'s real output.
- **The harness is deleted**: the `pairs` module and
  `sysstat_pairs_agree_or_skip_with_the_reason` (its own `Pair`, runner and
  path grammar). Nothing in the module runs a tool.
- **Kept, by decision**: `defect_hold::poll_leading_event_is_held_as_a_known_defect`,
  which runs `omn` alone (no tool) to hold defect 2 in both directions; it
  is what keeps `raw_means`' skip honest, and no pair can hold it (the
  twenty-second wave's review: the leading event appears in some runs only,
  so a `known_defect` pair would flip).
- `tests/reference/captures/sysstat/`: `mpstat.txt`, `sar-u.txt`,
  `iostat-c.txt`, `pidstat-r.txt`, `pidstat-w.txt` replaced by this
  machine's real output (sysstat 12.8.0, `LC_ALL=C`, 24 cpus), parser tests
  moved to their numbers; new `omn-cpu-util-raw.json`, `omn-mem-swap-raw.json`,
  `omn-cpu-pressure-raw.json`, `omn-io-pressure-raw.json` (`omn` today,
  `-d 2s`). The cpu.util capture carries the few-ms leading event (225 us
  in, busy 0.0), so the skip is tested on a real one.
- `docs/replaces/sysstat.md`: every row backed by a pair id or `no pair:`,
  numbers from the runner's run; the `iostat -d`/`-x` row (red under
  `every_row_names_a_pair_or_why_none` before) now says `no pair:` and points
  at io's table.

## Each pair

`cargo test --test reference pairs_sysstat -- --nocapture`, once, 61 s,
with other streams loading the machine:
`tally: reference sysstat: 12 held, 0 skipped, 0 gaps, 1 known defects, 0 failed`.

| Pair | Outcome | Seen | Tolerance |
|---|---|---|---|
| `numastat-p` | holds | 35 236 vs 35 253.8 pages | rel 0.03 |
| `pidstat-r` | holds | 144 306 176 vs 143 654 912 B (+0.45 %) | rel 0.03 |
| `pidstat-w` | known defect (defect 1) | 77 135/s vs 931/s | rel 0.15 |
| `vmstat-busy` | holds | 0.1489 vs 0.145 | abs 0.03 |
| `vmstat-si` | holds | 0 vs 0 | abs 256 KiB/s |
| `vmstat-so` | holds | 0 vs 0 | abs 256 KiB/s |
| `psi-cpu-some` | holds | 0.05891 vs 0.05887 | abs 0.02 |
| `psi-memory-some` | holds | 0 vs 0 | abs 0.02 |
| `psi-io-some` | holds | 0.0834 vs 0.0864 | abs 0.02 |
| `psi-io-full` | holds | 0.0631 vs 0.0617 | abs 0.02 |
| `mpstat-all` | holds | 0.4210 vs 0.4226 | abs 0.03 |
| `iostat-c` | holds | 0.5207 vs 0.5139 | abs 0.03 |
| `sar-u` | holds | 0.8959 vs 0.9082 | abs 0.03 |

Every number agrees with what the twenty-second wave's own harness showed:
the runner reads the same values, so no pair is a defect of the runner or of
a pair. No tolerance was widened.

## Shown vs reasoned

- **Shown**: the tally above; `cargo test --test reference sysstat::`
  16 passed (15 parser tests, the defect-2 hold); `every_pair_is_well_formed`
  names no sysstat pair (it is red for other families); 
  `every_row_names_a_pair_or_why_none` names no sysstat row (the remaining
  rows it names that mention sysstat are top's, pointing here);
  `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo fmt --check` clean.
- **Shown, the defects still reproduce on today's `omn`**: defect 1,
  `sched.switches -m count -e rate -- switch.py 1000` gave `total` 1016,
  `window_ns` 16 953 550, `per_second` 59 928 (and 77 135 under the runner);
  defect 2, the cpu.util capture's first event 225 us after the window
  opened, busy 0.0, and the hold test saw it again.
- **Reasoned, not shown**: the swap and memory-pressure pairs agree only at
  zero (no swapping, no memory pressure here); a machine that swaps would
  show whether `in_bytes` counts zero-filled swap-ins that vmstat's `pswpin`
  does not. The pairs ran once, not on a quiet machine; the coordinator's
  full suite is the verdict.

## Defects in omn the pairs hold (unchanged from the twenty-second wave)

1. `sched.switches`' counted-window rate divides by a 12-22 ms window, not
   the run's seconds: `pidstat-w`, `known_defect`.
2. A poll's leading raw event covers a few ms (`cpu.util`, `cpu.pressure`):
   left out by `raw_means`, held by `poll_leading_event_is_held_as_a_known_defect`.
3. `cpu.pressure` names its attributes unlike its siblings and has no `full`
   share: `omn_psi` reads both namings; no `psi-cpu-full` pair.

## Not done, and why

- No new pairs for the gaps of `docs/replaces/sysstat.md` (`free`, `uptime`,
  `slabtop`, system `numastat`, interrupt counts, `pidstat -u`/`-d`,
  `mpstat -P ALL`): out of this wave's scope, and most need a source in
  `src/` first (`docs/fixes/w22-sysstat.md`, defects 4 to 10).
- The pairs were not rerun for flakiness: one run held everything, and the
  brief keeps the repeat for the coordinator's quiet machine. The
  twenty-second wave saw `io.pressure` `some` miss once by 0.025 under heavy
  I/O from other streams (passed on its rerun); if `psi-io-some` goes red
  under load, rerun it alone once
  (`OMN_REFERENCE_PAIR=psi-io-some cargo test --test reference pairs_sysstat -- --nocapture`).
- No `make check`, no `cargo test --workspace`, per the brief.

## Privileged runs wanted

None: every sysstat pair runs without root. A future `slabtop` pair would
need `sudo slabtop -o -s c | head -20` and a `mem.slab` source.

## Decision lines

- A system pair is `run: alongside` with `settle_ms: 200` when a workload is
  let go, so both pollers have opened their window before it starts.
- A poll's raw shares are compared as their mean, through an `omn_parse`
  parser, with defect 2's few-ms leading event left out; the leaving-out is
  held by a test that runs `omn` alone, which is the one test in the module
  that runs a binary.

## Needs from another stream

- **src (after the wave)**: defects 1 to 3; once defect 2 is fixed, drop the
  skip in `raw_means` and the hold test together, and add a `summary.mean`
  pair (a path, no parser) for `cpu.util` against `mpstat`.
- **Coordinator**: none beyond the full run; the `docs/replaces.md` index
  can take `sysstat: 12 held, 1 known defect`.
