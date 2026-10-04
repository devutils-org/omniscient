# The twenty-third wave, stream perfstat

`perf stat` against `omn trace <source> -m count -e summary`, every pair now
the shared runner's. Branch `feat/w23-perfstat`: `b209ec4` (the pairs, the
parser test, the table, `run_pairs.py` gone) and this handoff.

## What landed

- `catalog/references/perfstat.yaml`: the thirteen pairs rewritten to the
  README's schema, each with an `id`. Every `omn` side is the path
  `results.0.outputs[emit=summary].series.*.last` (the sum of the summary's
  series: one per process, per cpu with `--system`, per cgroup with
  `--cgroup`), or `results.0.lifetime.cpu_ns` with `omn_scale: 0.000001`
  for task-clock. No `omn_jq`, no `same_run`, no `setup: <name>`; every
  `omn` argv carries `-o {out}`. No `omn_parse` was needed, so the module
  holds no parser of `omn`'s output.
- How each runs, and why it changed from the twenty-second wave:
  - per process (`instructions`, `ipc`, `branch-misses`, `cache-misses`,
    `page-faults`, `context-switches`, `cpu-migrations`): `run: separate`,
    `runs: 3`, five for `branch-misses` and `cache-misses`;
  - `cpu.cycles` and `task-clock`: `run: omn_launches_tool` (omn launches
    `perf stat`, which launches the workload), `side: above`. Separate runs
    differed by 9 % under the wave's load, a median of three included;
  - `cpu.cycles.system`, `cpu.context_switches.system`:
    `run: tool_wraps_omn`, `side: below` (omn's window inside perf's).
    `perf stat -a` opens per-cpu events, not inherited ones, so the
    capability caveat of the README does not apply;
  - `cpu.instructions.pid`: `run: attach`, `omn --pid {pid} -d 2s` and
    `perf stat -p {pid}`; both count the workload's whole life, so the raw
    count is compared, no longer the rate per on-cpu nanosecond;
  - `cpu.instructions.cgroup`: `run: alongside` with a `setup` that makes a
    child cgroup of the test's own (`omn-reference-perfstat`, user-owned
    under systemd's delegation) and links it as `{tmp}/cg`; the workload
    moves itself in by the same fixed name. Both tools take
    `../../..{tmp}/cg` (perf resolves `-G` under `/sys/fs/cgroup`; omn
    accepts the same spelling).
- `tests/reference/perfstat.rs`: one more test,
  `csv_reads_a_cgroup_named_relative_to_the_mount`, on a new capture
  `cgroup-relative-csv.txt` (real `perf stat -a -G ../../../<dir>/cg`
  output from this machine; the scratch directory's path in it rewritten to
  the runner's `/tmp/omn-reference-<pid>-<n>-<m>` shape). The six earlier
  captures were already `perf`'s real output on this machine; none was
  written from documentation.
- `tests/reference/workloads/perfstat/run_pairs.py` deleted.
- `docs/replaces/perfstat.md`: every row's verdict is the runner's, each
  backed by a pair id or `no pair:` and why.

## Shown vs reasoned

Shown: `make`, then `cargo test --test reference pairs_perfstat --
--nocapture` five times under the wave's load, the last three with every
pair in its final shape: **13 held, 0 skipped, 0 gaps, 0 known defects, 0
failed** on each of those three (the first two runs found the shapes below).
Worst deviation per pair across the runs in final shape:

| Pair | omn vs perf |
|---|---|
| `cpu.instructions` | -0.015 % |
| `cpu.cycles` | +2.8 to +3.5 % (perf's own cycles) |
| `cpu.ipc` | -4.6 to +1.6 % |
| `cpu.branch_misses` | -1.5 % (five runs' medians) |
| `cpu.cache_misses` | -2.6 % (five runs' medians) |
| `mem.faults` | +5 or +6 of ~970, every run |
| `cpu.context_switches` | 0 to -2 of ~2005 |
| `cpu.migrations` | 0 to +3 of ~503 |
| `task-clock` | +2.7 to +3.1 % (perf's own cpu time) |
| `cpu.cycles.system` | -1.7 to -7.4 % (band -8 %) |
| `cpu.context_switches.system` | -2.1 to -5.7 % (band -15 %) |
| `cpu.instructions.cgroup` | +636 to +1431 of 4.75e9 |
| `cpu.instructions.pid` | +159 to +11 713 of 4.75e9 |

Shown by the first two runs, and why the shapes changed:
`cpu.cycles` separate with medians of three: +9.3 %; `cpu.cache_misses`
separate, medians of three: -41 % (perf's median high through a load
spike); `task-clock` separate: -9.4 %; `cpu.branch_misses` separate,
medians of three: +4.8 %. By hand: omn launching `perf stat` adds perf's
own share to omn's count, 2.3 % of cpu time, but 18 % of branch misses and
about 40 % of cache misses, so those two stay separate with five runs.

Also shown: `perf stat -p` ends when the pid exits, but `omn --pid ...
-m count` without `-d` refuses: "a poll needs to know when to stop: pass
--duration, or launch a program with `-- prog`" (a row's "Missing").

`cargo test --test reference perfstat::` 9 passed;
`every_pair_is_well_formed` and `every_row_names_a_pair_or_why_none` name
nothing of perfstat (both still red on other families not yet merged);
`cargo clippy --workspace --all-targets -- -D warnings` and
`cargo fmt --check` clean.

Reasoned, not shown: that the separate pairs hold on a quiet machine the
way they did here under load (they should hold more easily); that
`cpu.cycles.system` keeps inside its band -- its -7.4 % against -8 % is
the closest pair, and it depends on how long omn takes to start and exit
under load, which is the band's mechanism.

## Not done, and why

- The cgroup `omn-reference-perfstat` stays behind, empty, in the test
  process's cgroup after the run: `setup` must finish and the runner has no
  teardown, so it is removed and remade by the next run's `setup`. Harmless
  (no process, no controller), and it goes with the test's scope.
- No pair for `perf stat -I`, `-A`, `--per-thread`, `--topdown`, `-r`,
  `branches`, the default set: unchanged from the twenty-second wave, each
  row says why.
- No `make check`, no `cargo test --workspace`, per the brief.

## Privileged runs wanted

None: every pair needs only `perf` and `perf_event_paranoid` -1. Unchanged
from the twenty-second wave, for `cpu.oncpu_time`'s `trace` backend:

```console
sudo ./target/debug/omn trace cpu.oncpu_time -m trace -e summary --format json -- \
  env PYTHONHASHSEED=0 python3 -S tests/reference/workloads/perfstat/spin.py 3000000
```

## Decision lines

- A value that follows the clock or the machine's load (cycles, cpu time)
  is compared in one run, omn launching perf, with a one-sided band: perf's
  own share is the only difference, and it only adds to omn's.
- A count perf's own process would swamp (branch and cache misses) stays on
  separate runs, five medians, and the quiet machine's suite is its verdict.
- By pid and per cgroup the workload runs inside both windows, so raw
  counts are compared; the rate per on-cpu nanosecond is gone.
- A cgroup the runner does not make is made by `setup` under the test's own
  cgroup and reached through `{tmp}`, rather than declared a gap.

## Needs from another stream

- **runner (later)**: a `teardown:` argv, run after the pair whatever
  happened, would let `cpu.instructions.cgroup` remove its cgroup.
- **src (later)**: `omn --pid` ending when the process exits, as `perf stat
  -p` does (today `-m count` with `--pid` refuses without `-d`); the
  twenty-second wave's gaps 1 to 5 stand (`cpu.branches`, several sources
  on one command line, `--system`/`--cgroup` beside a launch, task-clock as
  a source, a repeat count).
- **Coordinator**: none of this family's lines are new skips, so
  `.github/expected-skips.txt` needs nothing (a cell without `perf` skips on
  the existing "is not installed here" line).
