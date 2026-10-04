# What omn replaces: the `perfsched` family

`perf sched`, `perf lock`, `perf trace -s`, `strace -c` and `strace -T`,
against `proc.syscall`, `sched.switches`, `sched.migrations`,
`sched.runqlat` and `sched.lock`. The pairs are in
`catalog/references/perfsched.yaml`, the parsers in
`tests/reference/perfsched.rs`; a pair is named here by its `id`, and every
verdict below is the shared runner's (`cargo test --test reference
pairs_perfsched`, 2026-10-04).

On this machine `perf sched`, `perf trace` and `perf lock` cannot run
unprivileged (`/sys/kernel/tracing` is mode 755 now, but each event's `id`
is `-rw-r----- root`, and `perf trace -s` and `perf lock contention -b` load
BPF, which needs `CAP_BPF`; `sudo -n` is not granted), so their pairs are
`privileged: true` and skip; their parsers are tested on captures
written in the tools' documented layouts, not produced here (four captures: `perf-trace-s.txt`,
`perf-sched-latency.txt`, `perf-sched-timehist-s.txt`,
`perf-lock-contention.txt`). Until they are replaced by real ones, a perf
pair is not to be trusted. The `strace` pairs run.

| Tool and use | omn equivalent | Covered | Same numbers | Missing |
|---|---|---|---|---|
| `strace -c -f -- prog`: calls per syscall | `omn trace proc.syscall -m count -e summary --filter 'syscall == NAME'` | in part: one syscall a run, the count exact | **yes**, exactly: 5000 `getppid` both sides (pair `strace-c-count`, `abs: 0`: 5000 against 5000 through the runner) | `syscall` as a key of `summary`/`topk`, so one run gives the whole table (`raw` carries it per row, the summaries key by task only); omn's launcher counts its own PATH search as the program's (12 failed `execve` and one `close`, `ioctl`, `lseek` more than strace) |
| `strace -c -w`: wall time per syscall | the same, `summary.mean` | in part, as above | **within 2 %**: 10.054-10.067 ms against 10.056 ms for a 10 ms sleep (pair `strace-c-latency`, `rel: 0.02`, medians of five runs: through the runner 10.056 ms against 10.055 ms; with three runs under a load average of 16, 10.10 against 10.12 ms) | as above. Short calls disagree by design: strace's stop is in the call (`getppid` 16 us under strace, 0.25 us under omn), omn is the right one |
| `strace -c` without `-w`: system cpu time per syscall | none | no | no pair: omn times a call from entry to exit, not the cpu it cost | per-syscall cpu time (the task's `se.sum_exec_runtime` delta across the call), if anyone wants it |
| `strace -T`: each call's duration | `proc.syscall -m count -e summary` (mean), `-e hist` (distribution) | in part: the distribution, not each call | **within 2 %** on the mean, 10.05 ms both (pair `strace-t-latency`, `rel: 0.02`, medians of five runs; through the runner 10.055 ms against 10.077 ms, and alone 10.06-10.07 against 10.09-10.11 ms; a loaded machine wakes either side late by up to a millisecond) | a per-call record: `-m trace` on `proc.syscall` is drained into log2 buckets, so a single slow call's timestamp and duration are lost |
| `strace -e trace=... -p PID`: the calls and their decoded arguments | `proc.syscall --pid` | no: omn names and times calls, it does not decode arguments or return values | no pair | argument and return decoding (`-e raw` has `syscall` and `nr` only); errors per syscall (`strace -c`'s `errors` column) |
| `perf trace -s -- prog`: calls per syscall | as `strace -c` | in part, as `strace -c` | not run: skips here, `sudo -n` (pair `perf-trace-count`, `abs: 0`) | as `strace -c` |
| `perf trace -s`: latency per syscall | as `strace -c -w` | in part | not run: skips here, `sudo -n` (pair `perf-trace-latency`, `rel: 0.02`) | as above; `perf trace`'s live per-event stream is `strace -T`'s row |
| `perf sched latency`: switches per task | `omn trace sched.switches -m count -e topk` | yes | not run: skips here, `sudo -n` (pair `perf-sched-latency-switches`, `rel: 0.15`); omn alone: 105-117 switches for 100 sleeps, three runs | -- |
| `perf sched latency`: average and maximum scheduling delay | `omn trace sched.runqlat -e summary` | in part: omn's `summary` is the kernel's `run_delay` account, not perf's wakeup to sched-in | no pair. The two are different quantities (perf's is omn's `wake_to_run_ns`, about a microsecond a wait more, and present on `-e raw` rows only), and alternate runs put the mean anywhere from 2 to 178 us here (three runs, 2026-10-04): no band both holds and catches a broken source | a `summary` (or `hist`) over `wake_to_run_ns`, so perf's quantity has an omn value; and a same-run comparison (perf records the machine, omn attaches by pid), which the runner does not offer, so the band could be a distribution's and not the machine's load |
| `perf sched timehist -s`: sched-in count, run time, migrations per task | `sched.switches`, `sched.migrations -m count -e topk`, `cpu.util` | in part | not run: skips here, `sudo -n` (pairs `perf-sched-timehist-sched-in` `rel: 0.15` and `perf-sched-timehist-migrations` `rel: 0.1`); omn alone: 203-209 migrations for `migrate.py 200`, three runs | `sched.migrations` prints no series when it saw none, rather than a zero: a reader cannot tell "none" from "not measured", so the pair's workload makes a known, non-zero count instead |
| `perf sched timehist`: one line per switch with wait, delay and run time | `sched.wakeup`, `sched.offcpu -e raw` | in part: the waits are there, bucketed per window | no pair: the per-switch line is not a value to compare | an undrained per-switch event stream joining wakeup, wait and run, which `perf sched timehist` prints |
| `perf sched map`, `perf sched replay` | none | no | no pair | a per-cpu occupancy view of the switches (`omn view` lanes come closest); replay is out of scope |
| `perf lock contention -b`: contentions, total and max wait | `omn trace sched.lock -e summary` (and `-e stack` for the callers) | in part | not run: skips here, `sudo -n` (pair `perf-lock-contention`, `rel: 0.3`); omn alone: 6000-7400 contentions for the 8-thread mmap churn | a key by lock type (omn has `kind` and `flags` per row but `summary` keys by task) and by lock address (`perf lock contention -l`); sched.lock dropped 1 % of its rows on that churn, which it reports |
| `perf lock record/report` (lockdep) | none | no | no pair: needs a kernel built with `CONFIG_LOCKDEP` and `CONFIG_LOCK_STAT`; this one has neither (`/proc/config.gz`) | -- |
