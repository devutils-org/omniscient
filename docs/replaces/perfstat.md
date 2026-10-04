# What omn replaces: the `perfstat` family

`perf stat` against `omn trace <source> -m count -e summary`. The pairs are in
`catalog/references/perfstat.yaml`, the parsers in
`tests/reference/perfstat.rs`, the workloads in
`tests/reference/workloads/perfstat/`. Measured on the development machine
(24 cpus, `perf_event_paranoid` -1, perf 7.2.8) while the eleven other
streams of the wave were loading it, three runs per pair unless said.

Per process, `perf stat` and `omn` each launch the workload (alternate runs);
the workloads are sized by an iteration count, so what they count does not
depend on how long the machine takes. System-wide, per cgroup and by pid,
`perf stat` wraps the `omn` process and both count the same seconds
(`same_run: tool_wraps_omn`).

| Tool and use | omn equivalent | Covered | Same numbers | Missing |
|---|---|---|---|---|
| `perf stat -e instructions -- prog` | `omn trace cpu.instructions -m count -e summary -- prog` | full | yes: within 0.003 % (pair `cpu.instructions`, tolerance 1 %) | -- |
| `perf stat -e cycles -- prog` | `omn trace cpu.cycles -m count -e summary -- prog` | full | within 3 % on alternate runs, the clock's spread (pair `cpu.cycles`, tolerance 8 %) | -- |
| `perf stat -e cycles,instructions` (insn per cycle) | `omn trace cpu.ipc -m count -e summary -- prog` | full | within 0.4 % (pair `cpu.ipc`, tolerance 8 %) | -- |
| `perf stat -e branches` | none | none | no pair: no source counts branch instructions | a `cpu.branches` source on `PERF_COUNT_HW_BRANCH_INSTRUCTIONS`, and the miss ratio beside it |
| `perf stat -e branch-misses -- prog` | `omn trace cpu.branch_misses -m count -e summary -- prog` | full | within 0.9 % (pair `cpu.branch_misses`, tolerance 4 %) | the miss ratio over branches (needs `cpu.branches`) |
| `perf stat -e cache-misses -- prog` | `omn trace cpu.cache_misses -m count -e summary -- prog` | full | within 2.5 % on a shared last-level cache (pair `cpu.cache_misses`, tolerance 10 %) | `cache-references` and the miss ratio as one source (`mem.cache_llc` reads both, not compared here) |
| `perf stat -e page-faults -- prog` | `omn trace mem.faults -m count -e summary -- prog` | full | within 0.5 %: omn reads 4 to 7 more on every run (pair `mem.faults`, tolerance 3 %) | the minor / major split perf prints as `minor-faults`, `major-faults` (not checked) |
| `perf stat -e context-switches -- prog` | `omn trace cpu.context_switches -m count -e summary -- prog` | full | within 0.3 % (pair `cpu.context_switches`, tolerance 3 %) | -- |
| `perf stat -e cpu-migrations -- prog` | `omn trace cpu.migrations -m count -e summary -- prog` | full | within 1 % (pair `cpu.migrations`, tolerance 5 %) | -- |
| `perf stat -e task-clock -- prog` | the `lifetime.cpu_ns` of any launched run | in part | within 3 % on alternate runs (pair "task-clock", tolerance 8 %) | a source to ask for it: `cpu.oncpu_time` refuses `count` on a launch, and its `trace` mode needs tracefs, which is root-only here |
| `perf stat -- prog` (the default set in one run) | a session file with one request per source, as `examples/cpu-counters.yml` | in part | the counts are those of the rows above; the shared launch was not compared | several sources on one `omn trace` command line; `branches` |
| `perf stat -a -e cycles -- sleep N` (system-wide) | `omn trace cpu.cycles --system -d Ns -m count -e summary` | full | same run: perf 3 to 4 % above, its window being omn's plus omn's start and exit (pair `cpu.cycles` system-wide, tolerance 8 %); two separate runs differed 2.5x under the load | `--system` beside a program to launch, observing the machine for as long as it runs (`perf stat -a -- prog`); omn refuses the combination |
| `perf stat -a -e context-switches -- sleep N` | `omn trace cpu.context_switches --system -d Ns -m count -e summary` | full | same run: perf 2.5 to 5 % above on three runs (pair `cpu.context_switches` system-wide, tolerance 15 %) | as above |
| `perf stat -a -G cg -e instructions -- sleep N` (per cgroup) | `omn trace cpu.instructions --cgroup /sys/fs/cgroup/cg -d Ns -m count -e summary` | full | same run: perf 2.3 and 4 % above by hand, the window difference (pair `cpu.instructions` per cgroup, tolerance 6 %) | `--cgroup` beside a program to launch; the runner's `setup: cgroup_spin` |
| `perf stat -p PID -e instructions` | `omn trace cpu.instructions --pid PID -d Ns -m count -e summary` | full | per on-cpu ns within 1 %, omn 1 % above on 3 of 3 runs; raw counts 5 to 23 % apart by window length (pair `cpu.instructions` by pid, tolerance 3 %) | the runner's `setup: spin_pid` |
| `perf stat -I ms` (interval counts) | `omn trace <source> -m poll@ms -e timeline` | in part | not compared | a pair keyed per interval |
| `perf stat -A` / `--per-thread` | `--keys cpu` / `--keys task` on the same sources | in part | not compared | pairs keyed per cpu and per thread |
| `perf stat -r N` (repeat, mean and stddev) | none | none | no pair | a way to repeat one request N times and report the spread |
| `perf stat --topdown` | `omn trace cpu.topdown -m count -e summary -- prog` | in part | not compared: `--topdown` needs the slots events, not checked on this cpu | a pair, on a machine with Icelake or Zen 4 or later |
