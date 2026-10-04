# Wave 22, stream perfstat -- handoff

`perf stat` against `omn trace <source> -m count -e summary`.

## What landed

- `catalog/references/perfstat.yaml`: thirteen pairs.
  - Per process, alternate runs (perf and omn each launch the workload):
    `cpu.instructions`, `cpu.cycles`, `cpu.ipc`, `cpu.branch_misses`,
    `cpu.cache_misses`, `mem.faults`, `cpu.context_switches`,
    `cpu.migrations`, and task-clock against `lifetime.cpu_ns`.
  - Same run (perf wraps the omn process): `cpu.cycles` and
    `cpu.context_switches` with `--system`, `cpu.instructions` with
    `--cgroup`, `cpu.instructions` with `--pid` (compared per on-cpu
    nanosecond).
  - Family fields the runner may not know yet: `compare.omn_jq` (the exact
    jq expression `compare.omn` names), `same_run: tool_wraps_omn`,
    `setup: cgroup_spin | spin_pid` with `{cgroup}`, `{cgroup_abs}`,
    `{pid}` placeholders. Documented at the top of the YAML.
- `tests/reference/perfstat.rs`: parsers `perf_stat_csv` and
  `perf_stat_human`, eight unit tests on six captures under
  `tests/reference/captures/perfstat/`. The CSV parser keeps a multiplexed
  estimate and reports `<event>.enabled_pct`, leaves out `<not counted>` and
  `<not supported>` (absent, never zero), sums `cpu_core/X/` and
  `cpu_atom/X/`, handles the extra `-G` column, and derives `ipc` and
  `<event>.per_cpu_ns` when task-clock is present.
- Five workloads under `tests/reference/workloads/perfstat/`, sized by an
  iteration count, silent on stdout (omn's JSON shares it): `spin.py`,
  `branchy.py`, `touch.py`, `yield.py`, `hop.py`.
- `docs/replaces/perfstat.md`: nineteen rows.
- Fix round: `tests/reference/workloads/perfstat/run_pairs.py`, which runs
  every pair reading the family fields as the YAML header documents them
  (see "Review").

## Shown vs reasoned

Shown, by hand on this machine under the wave's load (the runner was not
on main yet, so no pair has run through `tests/reference.rs`):

| Pair | omn vs perf, measured | Tolerance |
|---|---|---|
| instructions | 0.003 % (3 runs) | rel 0.01 |
| cycles | -2.8 to +0.6 % | rel 0.08 |
| ipc | 0.4 % (1 run) | rel 0.08 |
| branch-misses | -0.9 to +0.6 % | rel 0.04 |
| cache-misses | -2.5 to -0.6 % | rel 0.10 |
| page-faults | +0.4 to +0.5 % (omn 4 to 7 more, every run) | rel 0.03 |
| context-switches | +0.05 to +0.25 % | rel 0.03 |
| cpu-migrations | +0.2 to +1.0 % | rel 0.05 |
| task-clock vs lifetime.cpu_ns | -2.9 to +0.6 % | rel 0.08 |
| cycles, --system, same run | perf 3 to 4 % above (2 runs) | rel 0.08 |
| context-switches, --system, same run | perf 2.5 to 5 % above (3 runs) | rel 0.15 |
| instructions, --cgroup, same run | perf 2.3 and 4 % above (2 runs) | rel 0.06 |
| instructions per cpu ns, --pid, same run | omn 0.8 to 1.0 % above (3 runs) | rel 0.03 |

Fix round, shown: every pair run straight from the YAML by
`run_pairs.py . 3`, three rounds, 39 runs, 0 failing. Worst deviation per
pair: instructions -0.01 %, cycles -0.9 %, ipc +1.7 %, branch-misses
-2.2 %, cache-misses -4.8 %, page-faults +0.5 % (omn 3 to 5 more, every
run), context-switches -0.2 %, cpu-migrations +1.4 %, task-clock -1.2 %,
cycles --system -5.3 %, context-switches --system -5.0 %, instructions
--cgroup -2.2 %, instructions per cpu ns --pid +0.47 %. Every one inside
its tolerance; the --system cycles pair is the closest, 5.3 against 8.

Reasoned, not shown: that `tests/reference.rs`, once it runs pairs, reads
these fields the way `run_pairs.py` does; the tolerances were set from three runs each,
which is a small sample.

Separate runs system-wide are useless under this load: omn and perf
differed by 2.5x on cycles. Hence `same_run`.

## Not done, and why

- No pair has gone through `tests/reference.rs`: on main after the merge
  it still only declares `Parser` and `Values`, no runner. Every pair has
  gone through `run_pairs.py` instead (above); it is not in the suite.
- `perf stat -I`, `-A`, `--per-thread`, `--topdown`, `minor-faults` /
  `major-faults`: rows say "not compared"; no pair written.
- No skip lines for `.github/expected-skips*.txt`: every pair needs only
  `perf`, which is installed, and `perf_event_paranoid` is -1 here.

## Gaps in omn (lines of work for the waves after)

1. No `cpu.branches` source (`PERF_COUNT_HW_BRANCH_INSTRUCTIONS`): perf
   stat's default `branches` line and the branch-miss ratio have no
   equivalent.
2. `omn trace` takes one source: `perf stat -e a,b,c -- prog` needs a
   session file (`examples/cpu-counters.yml`). Several sources on one
   command line, one launch.
3. `--system`, `--cgroup` and `--pid` refuse a program to launch, so
   `perf stat -a -- prog` (observe the machine for as long as the program
   runs) has no equivalent; only a fixed `-d`.
4. No task-clock as a source: `cpu.oncpu_time` refuses `count` on a launch,
   and its `trace` backend needs tracefs, root-only here. The number is in
   every run's `lifetime.cpu_ns`, but cannot be asked for.
5. No `perf stat -r N`: repeat one request and report mean and spread.
6. `mem.faults` reads 4 to 7 faults more than `perf stat -e page-faults`
   on the same workload, every run: the counter is opened earlier around
   the exec than perf's. Small and constant, worth one line in the
   source's caveats.
7. By pid, omn's instructions per on-cpu nanosecond are 0.4 to 1 % above
   perf's on six runs out of six: `overhead.observed_cpu_ns` and perf's
   task-clock do not cover quite the same time. Not a defect yet; a
   question.

## Privileged runs wanted

None for the pairs. For gap 4's `trace` backend, a run with tracefs
readable:

```console
sudo ./target/debug/omn trace cpu.oncpu_time -m trace -e summary --format json -- \
  env PYTHONHASHSEED=0 python3 -S tests/reference/workloads/perfstat/spin.py 3000000
```

## Decision lines

- Per-process pairs compare alternate runs of count-sized workloads;
  system, cgroup and pid pairs compare the same run, with the tool wrapping
  omn. The tolerance names which in `why`.
- An attach by pid is compared per on-cpu nanosecond, not as a raw count:
  the two windows differ by omn's own start and exit.
- A count perf could not make is absent, never zero.

## Needs from another stream

- **reference**: support in the runner, or reject with a reason, the
  family fields `same_run: tool_wraps_omn` (run `reference.command` with
  omn's argv appended, empty `workload`), `setup: cgroup_spin` (child
  cgroup of the test's own, user-owned, a spinning `spin.py` moved into it,
  `{cgroup}` relative to `/sys/fs/cgroup` and `{cgroup_abs}` substituted,
  removed after) and `setup: spin_pid` (`{pid}` of a background `spin.py`,
  killed after); and `compare.omn_jq` as the meaning of `compare.omn`,
  or a path syntax that can sum a summary's series and divide two fields.

## Review

The reviewer: no pair has run through a runner; the pairs depend on family
fields the runner does not support (`same_run: tool_wraps_omn`, `setup:
cgroup_spin | spin_pid`, `compare.omn_jq`); `compare.omn` holds
pseudo-paths such as `summary.total` that are not in omn's JSON.

- **Not run through a runner: right, and still true**, because the runner
  is not on main: after merging main, `tests/reference.rs` and
  `tests/reference/common.rs` declare the parser registry and nothing that
  reads a pair. Writing it is stream reference's job, not this stream's.
  What this stream can do is show the pairs pass as written:
  `tests/reference/workloads/perfstat/run_pairs.py` reads
  `catalog/references/perfstat.yaml` and interprets every family field
  exactly as the YAML's header documents it. All thirteen pairs pass, three
  rounds each ("Shown vs reasoned"). The script is also the concrete
  reading the runner is asked to match.
- **Unsupported family fields: right**, and already filed under "Needs
  from another stream". `same_run: tool_wraps_omn` carries the same
  meaning as in `catalog/references/power.yaml`, so the runner needs one
  implementation for both families.
- **Pseudo-paths: the notation is the brief's, kept on purpose.**
  `summary.total` is the example BRIEF.md's schema gives for `compare.omn`,
  and eleven pairs across the families use it. It names the value for a
  reader. It cannot be a literal path: the value is the sum of a
  summary's series (one per cpu with `--system`), and the pid pair divides
  two fields. That is why every pair carries `compare.omn_jq`, the exact
  expression, and the one `run_pairs.py` evaluates. Replacing `compare.omn`
  with the jq text would leave this family alone off the brief's notation;
  whether the runner evaluates `omn_jq` or a path syntax that can sum and
  divide is the open request to stream reference.
