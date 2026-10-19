# What omn replaces: the `perfstat` family

`perf stat` against `omn trace <source> -m count -e summary`. The pairs are in
`catalog/references/perfstat.yaml`, the parsers in
`tests/reference/perfstat.rs`, the workloads in
`tests/reference/workloads/perfstat/`; the shared runner
(`tests/reference/runner.rs`) runs them:

```console
make && cargo test --test reference pairs_perfstat -- --nocapture
```

The verdicts below are the runner's, on the development machine (24 cpus,
`perf_event_paranoid` -1, perf 7.2.8, Linux 7.2) while ten other streams of
the twenty-third wave loaded it: five runs of the family, the last three
with every pair in its final shape, 13 held each time.

How each pair runs:

- **Per process**, separate runs (`perf stat -- prog`, then `omn ... --
  prog`, three or five times, alternating, medians compared), on workloads
  sized by an iteration count.
- **Cycles and cpu time follow the clock and the load**, so separate runs
  differed by 9 % under the wave: there `omn` launches `perf stat`, which
  launches the workload (`run: omn_launches_tool`). Both count the same
  cycles; `omn` also counts `perf`'s own, so the tolerance is one-sided.
- **System-wide**, `omn --system` launches `sudo -n perf stat -a`, which
  launches the workload (`run: omn_launches_tool`): unprivileged, `perf -a`
  is refused at `perf_event_paranoid` 2, and wrapping `omn` through `sudo`
  would run `omn` itself as root (both pairs skipped that way until
  2026-10-19). `perf`'s window lies inside `omn`'s, so `omn` can only read
  more.
- **By pid and per cgroup**, both observers start first, the workload after
  (`run: attach`, `run: alongside`): both count the whole of it, and agree
  to a few thousand instructions in 4.7 billion.

| Tool and use | omn equivalent | Covered | Same numbers | Missing |
|---|---|---|---|---|
| `perf stat -e instructions -- prog` | `omn trace cpu.instructions -m count -e summary -- prog` | full | yes: within 0.02 % (pair `cpu.instructions`, tolerance 1 %) | -- |
| `perf stat -e cycles -- prog` | `omn trace cpu.cycles -m count -e summary -- prog` | full | yes, in one run: omn 2.8 to 3.5 % above, perf's own cycles (pair `cpu.cycles`, omn above by at most 8 %) | -- |
| `perf stat -e cycles,instructions` (insn per cycle) | `omn trace cpu.ipc -m count -e summary -- prog` | full | yes: within 4.6 % on separate runs, the clock's spread (pair `cpu.ipc`, tolerance 8 %) | -- |
| `perf stat -e branches` | none | none | no pair: no source counts branch instructions | a `cpu.branches` source on `PERF_COUNT_HW_BRANCH_INSTRUCTIONS`, and the miss ratio beside it |
| `perf stat -e branch-misses -- prog` | `omn trace cpu.branch_misses -m count -e summary -- prog` | full | yes: within 1.5 % on five runs' medians; one median of three was 4.8 % off under the load (pair `cpu.branch_misses`, tolerance 4 %) | the miss ratio over branches (needs `cpu.branches`) |
| `perf stat -e cache-misses -- prog` | `omn trace cpu.cache_misses -m count -e summary -- prog` | full | yes: within 2.6 % on five runs' medians; a load spike once moved a median of three by 70 % (pair `cpu.cache_misses`, tolerance 10 %) | `cache-references` and the miss ratio as one source (`mem.cache_llc` reads both, not compared here) |
| `perf stat -e page-faults -- prog` | `omn trace mem.faults -m count -e summary -- prog` | full | yes: omn reads 5 or 6 more of about 970, every run (pair `mem.faults`, tolerance 3 %) | the minor / major split perf prints as `minor-faults`, `major-faults` (not checked) |
| `perf stat -e context-switches -- prog` | `omn trace cpu.context_switches -m count -e summary -- prog` | full | yes: 0 to 2 apart of about 2005 (pair `cpu.context_switches`, tolerance 3 %) | -- |
| `perf stat -e cpu-migrations -- prog` | `omn trace cpu.migrations -m count -e summary -- prog` | full | yes: 0 to 3 apart of about 503 (pair `cpu.migrations`, tolerance 5 %) | -- |
| `perf stat -e task-clock -- prog` | the `lifetime.cpu_ns` of any launched run | in part | yes, in one run: omn 2.7 to 3.1 % above, perf's own cpu time (pair `task-clock`, omn above by at most 5 %) | a source to ask for it: `cpu.oncpu_time` refuses `count` on a launch, and its `trace` mode needs tracefs, which is root-only here |
| `perf stat -- prog` (the default set in one run) | a session file with one request per source, as `examples/cpu-counters.yml` | in part | no pair: the counts are those of the rows above; the shared launch was not compared | several sources on one `omn trace` command line; `branches` |
| `perf stat -a -e cycles -- prog` (system-wide) | `omn trace cpu.cycles --system -m count -e summary -- prog` | full | yes, in one run: omn 3.2 to 3.7 % above, perf's window inside omn's (pair `cpu.cycles.system`, omn above by at most 8 %) | -- |
| `perf stat -a -e context-switches -- sleep N` | `omn trace cpu.context_switches --system -m count -e summary -- sleep N` | full | yes, in one run: omn 1.9 to 4.2 % above (pair `cpu.context_switches.system`, omn above by at most 15 %) | -- |
| `perf stat -a -G cg -e instructions -- sleep N` (per cgroup) | `omn trace cpu.instructions --cgroup /sys/fs/cgroup/cg -d Ns -m count -e summary` | full | yes: 636 to 1431 apart of 4.75 billion (pair `cpu.instructions.cgroup`, tolerance 1 %) | `--cgroup` beside a program to launch |
| `perf stat -p PID -e instructions` | `omn trace cpu.instructions --pid PID -d Ns -m count -e summary` | full | yes: 159 to 11 713 apart of 4.75 billion (pair `cpu.instructions.pid`, tolerance 1 %) | an end with the process: `perf stat -p` stops when the pid exits, `omn --pid` with `-m count` refuses to start without `-d` ("a poll needs to know when to stop") |
| `perf stat -I ms` (interval counts) | `omn trace <source> -m poll@ms -e timeline` | in part | no pair: not compared | a pair keyed per interval |
| `perf stat -A` / `--per-thread` | `--keys cpu` / `--keys task` on the same sources | in part | no pair: not compared | pairs keyed per cpu and per thread |
| `perf stat -r N` (repeat, mean and stddev) | none | none | no pair: omn cannot repeat a request | a way to repeat one request N times and report the spread |
| `perf stat --topdown` | `omn trace cpu.topdown -m count -e summary -- prog` | in part | no pair: `--topdown` needs the slots events, not checked on this cpu | a pair, on a machine with Icelake or Zen 4 or later |
