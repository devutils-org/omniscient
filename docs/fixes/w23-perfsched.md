# The twenty-third wave, stream perfsched

`perf sched`, `perf lock`, `perf trace -s`, `strace -c` and `strace -T`,
moved onto the shared runner. Branch `feat/w23-perfsched`: `d9a574e` (the
pairs and the table), and this handoff.

## What landed

- `catalog/references/perfsched.yaml`: the nine pairs rewritten to
  `catalog/references/README.md`. Each has an `id` now (`strace-c-count`,
  `strace-c-latency`, `strace-t-latency`, `perf-trace-count`,
  `perf-trace-latency`, `perf-sched-latency-switches`,
  `perf-sched-timehist-migrations`, `perf-sched-timehist-sched-in`,
  `perf-lock-contention`). `omn: <emit>.<field>` plus `across: sum` became
  a path into `{out}/results.json`,
  `results[source=<src>].outputs[emit=<emit>].series.*.<field>` (the path
  sums over `*`). `scale: 1000` became `reference_scale: 1000`. Every `omn`
  argv got `-o {out}`. The two sleep-latency pairs that run (`strace-c-latency`,
  `strace-t-latency`) take `runs: 5`, and their `why` says why: a loaded
  machine wakes the sleeper late.
- Every `omn` argv was checked against today's `omn`, run by hand on the
  pair's workload: `proc.syscall -m count -e summary --filter 'syscall ==
  getppid'` (one series, `count` 5000), `sched.switches -m count -e topk`
  (`total` 102 for `sleeper.py 100 2`), `sched.migrations -m count -e topk`
  (`total` 205 for `migrate.py 200`), `sched.lock -e summary` (nine
  series, one per thread, `count` summing to 6782 for `contend.py 8 2`, with
  37 of 5253 rows dropped and reported).
- `docs/replaces/perfsched.md`: each row names its pair by `id`, with the
  runner's verdict. Rows whose pair skips say "not run: skips here".
- No harness to delete: `tests/reference/perfsched.rs` runs no tool and had
  no `mod runner` or ignored test. No `omn_parse` parser was needed, so the
  module and the captures are unchanged.

## Each pair's outcome (`cargo test --test reference pairs_perfsched`)

| Pair | Outcome |
|---|---|
| `strace-c-count` | holds: 5000 against 5000, `abs: 0` |
| `strace-c-latency` | holds: 10.056 ms against 10.055 ms, `rel: 0.02`, medians of five |
| `strace-t-latency` | holds: 10.055 ms against 10.077 ms, `rel: 0.02`, medians of five |
| `perf-trace-count`, `perf-trace-latency` | skipped: `sudo -n` does not grant root here |
| `perf-sched-latency-switches`, `perf-sched-timehist-sched-in`, `perf-sched-timehist-migrations` | skipped: `sudo -n` |
| `perf-lock-contention` | skipped: `sudo -n` |

Tally: `reference perfsched: 3 held, 6 skipped, 0 gaps, 0 known defects, 0 failed`.

## Shown vs reasoned

- **Shown**: the tally above, through the runner, after `make` (`omn`
  carrying `cap_perfmon,cap_bpf=ep`). The first full run went red on
  `strace-t-latency` with three runs: omn's median was 10.49 ms against
  strace's 10.05, at a load average of 16-22 while the other streams ran.
  Rerun alone it held twice (10.068 against 10.091 ms, 10.062 against
  10.107 ms). By hand, four alternate runs of each side gave 10.18, 10.07,
  10.99 and 10.10 ms for omn and 10.05, 10.06, 11.01 and 10.24 ms for
  strace: under load, single runs on either side land up to a millisecond
  late. The pair is not wrong and omn is not wrong; the fix is more runs, not a wider
  band (`runs: 5`, said in `why`).
- **Shown**: `every_pair_is_well_formed` and
  `every_row_names_a_pair_or_why_none` name nothing of perfsched (both
  still fail on other families' pairs and rows); `cargo test --test
  reference perfsched::` 11 passed; `cargo clippy --workspace --all-targets
  -- -D warnings` and `cargo fmt --check` clean.
- **Shown**: perf still cannot run here unprivileged, for a narrower reason
  than in the twenty-second wave. `/sys/kernel/tracing` is mode 755 now,
  but each event's `id` and `enable` are `-rw-r----- root` (`perf sched
  record`: "No permissions to read .../events/sched/sched_switch"), and
  `perf trace -s` and `perf lock contention -b` load BPF skeletons, refused
  with EPERM. The six perf pairs stay `privileged: true`.
- **Reasoned, not run**: every perf pair's tolerance, and the perf parsers.
  Their four captures (`perf-trace-s.txt`, `perf-sched-latency.txt`,
  `perf-sched-timehist-s.txt`, `perf-lock-contention.txt`) are still
  written in the tools' documented layouts, not produced here.

## Not done, and why

- The four perf captures were not replaced: perf cannot make them without
  root (above). The commands are below.
- No pair moved to a new mode. The perf pairs could run `alongside` (perf
  records the machine over the same seconds omn does), which would make
  `perf-lock-contention`'s `rel: 0.3` (a band for two separate churns) a
  same-run band. Not done without a run to show it holds: worth doing once
  root is granted.
- The `sched.runqlat` against `perf sched latency` average-delay pair stays
  absent (twenty-second wave, review 3): omn has no summary over
  `wake_to_run_ns`.

## Privileged runs wanted

```console
sudo -v    # by a person, once; or grant `sudo -n` for /usr/bin/perf alone
cd /home/moha/repos/omniscient-w23-perfsched && make
cargo test --test reference pairs_perfsched -- --nocapture
# real captures to replace the four documented-layout ones:
W=tests/reference/workloads/perfsched C=tests/reference/captures/perfsched
sudo perf trace -s -o $C/perf-trace-s.txt -- python3 $W/syscalls.py 5000 20 10
d=$(mktemp); sudo perf sched record -q -o $d -- python3 $W/sleeper.py 100 2 >/dev/null 2>&1
sudo perf sched latency -i $d > $C/perf-sched-latency.txt
sudo perf sched timehist -s -i $d > $C/perf-sched-timehist-s.txt
sudo perf lock contention -b -- python3 $W/contend.py 8 2 2> $C/perf-lock-contention.txt
```

After a capture is replaced, its parser test's expected numbers change
with it (`cargo test --test reference perfsched::`).

## Decision lines

- omn's side is a path, not a parser: every value perfsched compares is a
  field of the source's one emit summed over its series (`series.*`), which
  the grammar reaches. A single-threaded workload has one series, so
  `series.*.mean` is that task's mean; a second thread would add a second
  mean, which the pair would show as a doubled value rather than hide.
- The sleep-latency pairs take more runs under load, not a wider band.

## Needs from another stream

- **Coordinator**: the six perf pairs are verified only once root is
  granted (commands above); in the final suite they skip. `strace-t-latency`
  and `strace-c-latency` want the quiet machine the final run has.
- None from the runner: the schema covered every field this family used.
