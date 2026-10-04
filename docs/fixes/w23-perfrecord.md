# The twenty-third wave, stream perfrecord

`perf record`/`report`, `perf top`, `perf mem` and `perf c2c` against
`cpu.profile`, `mem.latency` and `mem.sharing`, now run by the shared runner.
Branch `feat/w23-perfrecord`: `b586a36` (the pairs, the parsers, the
workloads, the table) and this handoff.

## What landed

- `catalog/references/perfrecord.yaml`: five pairs (eight compares) in the
  runner's schema, each with an `id`:
  - `hot-functions` -- `perf record -p {pid}` + `perf report` (`report:` over
    `{tmp}/perf.data`) against `omn trace cpu.profile --pid {pid}`, both
    attached to one run (`run: attach`), keys `hot_beta`, `hot_alpha`,
    `hot_gamma`;
  - `stripped-library` -- the same on `python3 split.py`, key
    `libcrypto.so.3`;
  - `live-top` -- `perf top --stdio -p {pid}` against omn on the same pid,
    key `hot_beta`;
  - `load-latency` -- `perf mem record` + `perf script -F weight` against
    `mem.latency`'s histogram mean, separate runs, `runs: 5`;
  - `false-sharing` -- `perf c2c record` + `report` against `mem.sharing`'s
    top-k, separate runs, `runs: 3`, keys `top` and `0x4040c0` (the two w22
    pairs on one run are now one pair with two compares).
  Every omn side is an `omn_parse` parser (the prose `compare.omn` beside it
  is gone) reading the `results.json` of `-o {out}`.
- `tests/reference/perfrecord.rs`: the seven parsers unchanged; the
  family's own harness (`pairs_hold_on_this_machine` and its helpers) is
  deleted -- nothing in the module runs a tool.
- `tests/reference/captures/perfrecord/`: omn's three captures
  (`omn-profile-hot.json`, `omn-latency-chase.json`,
  `omn-sharing-share.json`) re-taken from today's `omn` with the pairs'
  argv. The `perf` captures were already real output (w22), not written
  from documentation; kept.
- `tests/reference/workloads/perfrecord/`: `build.sh --build NAME` builds and
  stops (each C pair's `setup`, so the pid the observers follow never runs
  `cc`); `hot.c` scales each round by a pseudo-random factor 1-8 (below).
- `docs/replaces/perfrecord.md`: the runner's numbers, each row citing its
  pair's id.

## Shown vs reasoned

Shown, `cargo test --test reference pairs_perfrecord -- --nocapture`, after
`make`, on the AMD Ryzen 9 3900X, kernel 7.2.8, `perf` 7.2.8,
`perf_event_paranoid` -1, with the other streams running (load 4-11). The
last two full runs of the committed pairs:

| Pair, key | omn | tool | allowed | |
|---|---|---|---|---|
| `hot-functions` `hot_beta` | 0.481, 0.514 | 0.511, 0.479 | 0.05 abs | held, held |
| `hot-functions` `hot_alpha` | 0.204, 0.197 | 0.195, 0.205 | 0.05 abs | held, held |
| `hot-functions` `hot_gamma` | 0.314, 0.288 | 0.284, 0.302 | 0.05 abs | held, held |
| `stripped-library` `libcrypto.so.3` | 0.594, 0.594 | 0.592, 0.601 | 0.05 abs | held, held |
| `live-top` `hot_beta` | 0.505, 0.500 | 0.489, 0.498 | 0.05 abs | held, held |
| `load-latency` (mean cycles) | -, 274.8 | -, 277.4 | 10 % | **failed** (below), held |
| `false-sharing` `top`, `0x4040c0` | 1.000, 1.000 | 1.000, 1.000 | 0.02 abs | held, held |

Tallies: `4 held, 0 skipped, 0 gaps, 0 known defects, 1 failed`, then
`5 held, 0 skipped, 0 gaps, 0 known defects, 0 failed` (and a third full
run before the last fix: 5 held). Parsers: `cargo test --test reference
perfrecord::`, 8 passed. `every_pair_is_well_formed` and
`every_row_names_a_pair_or_why_none` name nothing of perfrecord (both are
red on other families' pairs). `cargo clippy --workspace --all-targets -- -D
warnings` and `cargo fmt --check`: clean.

What the runner showed that the w22 harness did not, each fixed in the pair
or the workload:

- **omn takes more than 500 ms to attach.** With the default `settle_ms`,
  `stripped-library` read libcrypto 0.515 for omn against 0.577 for perf:
  omn missed the start of `split.py`, whose shares are fixed by phase. Every
  attach pair now has `settle_ms: 2000` and an omn `-d` covering the settle
  and the run (then 0.593 vs 0.583, 0.593 vs 0.579). A defect in the pair.
- **`hot.c` phase-locked with a 997 Hz sampler.** A round lasted about a
  millisecond, close to the sampler's period, so a periodic sampler walked
  slowly through the round and over-counted one function: by hand, six runs
  each, `hot_beta` read 0.46-0.57 on `perf record -e cpu-clock`, 0.47-0.54
  on omn, and `hot_gamma` 0.361 for omn against 0.272 for perf in one
  attached run. After scaling each round by a random factor (the shares
  unchanged), six runs read 0.49-0.51 (cpu-clock) and 0.48-0.52 (cycles).
  A defect in the workload, not in omn or perf.
- **`perf top` needs its window inside the run**: under `attach` it starts
  with omn, 2 s before the program; it now runs 5 s on a 4 s program
  (first attempt: "perf top drew no screen with a sampled row", then 0.67 on
  a one-second window).

Reasoned, not run: the w22 rows marked reasoned (`--children`, `omn top
--view flame`, the system and cgroup targets) stay so.

## Each pair's outcome

- `hot-functions`, `stripped-library`, `live-top`, `false-sharing`: **hold**.
- `load-latency`: **holds on a quieter machine, failed in 1 of the last 2
  full runs and in 3 of 3 runs alone during a busy spell** (omn 287 vs 320,
  431 vs 492, 425 vs 483 cycles: omn 10-12 % below `perf mem`, the
  tolerance 10 %). Not marked `known_defect` (it holds in most runs, and the
  marker would then fail) and not widened: the cause is open, below. Its
  `why` says it wants a quiet machine; the coordinator's quiet full run is
  its verdict.

No pair skips here; no gap.

### A finding: `mem.latency` and `perf mem` time different populations

Same workload (`chase 2`), same nominal rate, on one machine:

- omn's IBS sampler tagged **5742 ops in 2.48 s** (its own note: "IBS
  tagged 5742 ops ... 1919 were loads, 1420 of them timed"), about 2300 per
  second at `sample@4000hz` (`period_ns` 250000); `perf mem record` tagged
  10 750 ops in about 2.6 s, 2655 of them timed loads. omn samples at about
  58 % of the rate it was asked for, and nothing in its output says so.
- Of the timed loads, omn calls 18 % `peer` (262; mean about 38 cycles,
  L3-like) and 81 % `ram`; `perf mem` calls 6 % a cache hit and 94 % RAM.
  omn's RAM-only mean (`--filter 'level == "ram"'`) read 341-356 cycles
  against `perf mem`'s 276-289 over all timed loads.

So the two means are over different mixes of L3 and RAM hits, and the mix
moves with the load. Whether omn's level decoding or its rate is the defect
is not settled here. For `docs/open-findings.md` (coordinator), and a line
of work for `src/` (the IBS backend): say the delivered rate, and check
`peer` against `perf mem`'s data source on the same op.

## Not done, and why

- **`perf mem` and `perf c2c` on one run**: `perf mem record -p PID` fails
  here (`Failed to collect 'ibs_op//' for the '184540' workload: No such
  file or directory`), so those pairs stay `separate` with medians.
- **The `perf` captures** were not re-taken: they are real `perf` output from
  w22 and the parser tests' numbers are theirs; `perf-report-hot.txt` and
  `perf-top-hot.txt` predate the new `hot.c`, which changes nothing a parser
  reads.
- The w22 gaps 1-9 (`docs/fixes/w22-perfrecord.md`) are untouched: they are
  features for later waves.
- No `make check`, no `cargo test --workspace`, per the brief.

## Privileged runs wanted

None: every pair runs unprivileged at `perf_event_paranoid` -1. On a machine
at the kernel's default (2), `perf` needs `CAP_PERFMON` (w22's command
stands).

## Decision lines

- The profile pairs attach both observers to one run, so the shares compared
  come from the same execution; only the sampling clocks differ (perf's
  cycles, omn's cpu-clock), which is what the 0.05 band is for.
- Two compares on one run are one pair with a list of compares, not two
  pairs running the workload twice.
- A C workload is built in `setup` (`build.sh --build`), never inside the
  observed pid.
- A sampled workload must not be periodic at a period near the sampler's.

## Needs from another stream

- **Coordinator**: file the `mem.latency` / `perf mem` population finding
  above in `docs/open-findings.md`, and read `load-latency` from the quiet
  full run. Stray `perf.data` and `perf.data.old` (incomplete headers,
  written 21:39) appeared once in this worktree's root during a busy
  stretch and did not reappear in the last family run; deleted, cause not
  found.
- **src (later)**: the IBS backend's delivered rate against the requested
  one, surfaced; `omn trace --pid` attaching faster than 500 ms (the debug
  build), or saying when it is attached.
