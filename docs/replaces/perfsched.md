# What omn replaces: the `perfsched` family

`perf sched`, `perf lock`, `perf trace -s`, `strace -c` and `strace -T`,
against `proc.syscall`, `sched.switches`, `sched.migrations`,
`sched.runqlat` and `sched.lock`. The pairs are in
`catalog/references/perfsched.yaml`, the parsers in
`tests/reference/perfsched.rs`; a pair is named here by its tool and key.

On this machine `perf sched`, `perf trace` and `perf lock` cannot run
unprivileged (`/sys/kernel/tracing` is `drwx------ root`, and `sudo -n` is
not granted), so their pairs skip; their parsers are tested on captures
written in the tools' documented layouts, not produced here. The `strace`
pairs run.

| Tool and use | omn equivalent | Covered | Same numbers | Missing |
|---|---|---|---|---|
| `strace -c -f -- prog`: calls per syscall | `omn trace proc.syscall -m count -e summary --filter 'syscall == NAME'` | in part: one syscall a run, the count exact | **yes**, exactly: 5000 `getppid` both sides (pair `strace -c`, `getppid.calls`, `abs: 0`) | `syscall` as a key of `summary`/`topk`, so one run gives the whole table (`raw` carries it per row, the summaries key by task only); omn's launcher counts its own PATH search as the program's (12 failed `execve` and one `close`, `ioctl`, `lseek` more than strace) |
| `strace -c -w`: wall time per syscall | the same, `summary.mean` | in part, as above | **within 2 %**: 10.054-10.067 ms against 10.056 ms for a 10 ms sleep (pair `strace -c`, `clock_nanosleep.usecs_per_call`, `rel: 0.02`) | as above. Short calls disagree by design: strace's stop is in the call (`getppid` 16 us under strace, 0.25 us under omn), omn is the right one |
| `strace -c` without `-w`: system cpu time per syscall | none | no | no pair: omn times a call from entry to exit, not the cpu it cost | per-syscall cpu time (the task's `se.sum_exec_runtime` delta across the call), if anyone wants it |
| `strace -T`: each call's duration | `proc.syscall -m count -e summary` (mean), `-e hist` (distribution) | in part: the distribution, not each call | **within 2 %** on the mean, 10.05 ms both (pair `strace -T`, `clock_nanosleep.mean_ns`, `rel: 0.02`) | a per-call record: `-m trace` on `proc.syscall` is drained into log2 buckets, so a single slow call's timestamp and duration are lost |
| `strace -e trace=... -p PID`: the calls and their decoded arguments | `proc.syscall --pid` | no: omn names and times calls, it does not decode arguments or return values | no pair | argument and return decoding (`-e raw` has `syscall` and `nr` only); errors per syscall (`strace -c`'s `errors` column) |
| `perf trace -s -- prog`: calls per syscall | as `strace -c` | in part, as `strace -c` | skips here (needs tracefs; pair `perf trace -s`, `getppid.calls`, `abs: 0`) | as `strace -c` |
| `perf trace -s`: latency per syscall | as `strace -c -w` | in part | skips here (pair `perf trace -s`, `clock_nanosleep.mean_ns`, `rel: 0.02`) | as above; `perf trace`'s live per-event stream is `strace -T`'s row |
| `perf sched latency`: switches per task | `omn trace sched.switches -m count -e topk` | yes | skips here (pair `perf sched latency`, `omnref-sleep.switches`, `rel: 0.15`); omn alone: 105-117 switches for 100 sleeps, three runs | -- |
| `perf sched latency`: average and maximum scheduling delay | `omn trace sched.runqlat -e summary` | yes; omn also gives the histogram and the kernel's own `run_delay` account | skips here (pair `perf sched latency`, `omnref-sleep.avg_delay_ns`, `abs: 200000`); omn alone: a 256-565 us mean under this wave's load | the band is wide because the runs alternate; a same-run comparison (perf records the machine, omn attaches by pid) is what would tighten it |
| `perf sched timehist -s`: sched-in count, run time, migrations per task | `sched.switches`, `sched.migrations -m count -e topk`, `cpu.util` | in part | skips here (pairs `perf sched timehist`, `omnref-sleep.sched_in` `rel: 0.15` and `omnref-sleep.migrations` `abs: 5`) | `sched.migrations` prints no series when it saw none, rather than a zero (the pair reads it as `missing: zero`) |
| `perf sched timehist`: one line per switch with wait, delay and run time | `sched.wakeup`, `sched.offcpu -e raw` | in part: the waits are there, bucketed per window | no pair: the per-switch line is not a value to compare | an undrained per-switch event stream joining wakeup, wait and run, which `perf sched timehist` prints |
| `perf sched map`, `perf sched replay` | none | no | no pair | a per-cpu occupancy view of the switches (`omn view` lanes come closest); replay is out of scope |
| `perf lock contention -b`: contentions, total and max wait | `omn trace sched.lock -e summary` (and `-e stack` for the callers) | in part | skips here (pair `perf lock contention`, `total.contended`, `rel: 0.3`); omn alone: 6000-7400 contentions for the 8-thread mmap churn | a key by lock type (omn has `kind` and `flags` per row but `summary` keys by task) and by lock address (`perf lock contention -l`); sched.lock dropped 1 % of its rows on that churn, which it reports |
| `perf lock record/report` (lockdep) | none | no | no pair: needs a kernel built with `CONFIG_LOCKDEP` and `CONFIG_LOCK_STAT`; this one has neither (`/proc/config.gz`) | -- |
