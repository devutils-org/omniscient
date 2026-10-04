# Handoff: stream perfsched (twenty-second wave)

`perf sched` (latency, timehist), `perf lock`, `perf trace -s`, `strace -c`
and `strace -T` against omn. Branch `feat/w22-perfsched`.

## What landed

- `catalog/references/perfsched.yaml`: nine pairs. Three run unprivileged
  (`strace -c` count, `strace -c -w` latency, `strace -T` latency); six
  need tracefs or BPF for the tool and are `privileged: true` (`perf trace
  -s` x2, `perf sched latency` switches, `perf sched timehist -s` sched-in
  and migrations, `perf lock contention -b`). The `perf sched latency`
  average-delay pair was removed in review (see "Review").
- `tests/reference/perfsched.rs`: six parsers (`strace_c`, `strace_t`,
  `perf_trace_summary`, `perf_sched_latency`, `perf_sched_timehist_summary`,
  `perf_lock_contention`), eleven unit tests, every parser refusing an empty
  output rather than reading zero.
- `tests/reference/captures/perfsched/`: `strace-c.txt`, `strace-c-w.txt`,
  `strace-T.txt` captured here on the workload; `perf-trace-s.txt`,
  `perf-sched-latency.txt`, `perf-sched-timehist-s.txt`,
  `perf-lock-contention.txt` **written in the tools' documented layouts**,
  not produced here (perf cannot reach tracefs unprivileged on this
  machine). They should be replaced by real captures once the privileged
  runs below are made.
- `tests/reference/workloads/perfsched/`: `syscalls.py` (N `getppid`, M
  sleeps: syscalls nothing else in the interpreter makes), `sleeper.py`
  (renames itself `omnref-sleep` so a system-wide `perf sched record` can be
  read for it alone), `contend.py` (threads churning one process's
  `mmap_lock`), `migrate.py` (renames itself `omnref-migrate`, then
  alternates its affinity between two cpus N times: N migrations the kernel
  makes before each call returns).
- `docs/replaces/perfsched.md`: fourteen rows.

## Shown vs reasoned

Shown, by hand, on this machine (the runner was not on `main` when this
stream ran, so the pairs were exercised by running both sides and comparing,
not through `tests/reference.rs`):

- `getppid` count: omn 5000, `strace -c` 5000, three runs each. Exact.
- 10 ms sleep: omn mean 10.054-10.067 ms, `strace -c -w` 10.056 ms,
  `strace -T` 10.049-10.051 ms, three runs each. Within 0.2 %; the pairs
  declare 2 % for the load.
- Every syscall of the workload, omn (`-e raw`, summed by `syscall`) against
  `strace -c -f`: identical except `execve` 13 vs 1, and `close`, `ioctl`,
  `lseek` one more each in omn. The extra `execve` are omn's launcher
  searching `PATH` (the twelve directories before `/usr/bin` fail with
  ENOENT); strace resolves the path itself and execs once.
- omn alone on the sched workloads: `sched.switches` 105-117 for 100 sleeps,
  `sched.migrations` 0-2, `sched.runqlat` mean 256-565 us, `sched.lock`
  6000-7400 contentions for `contend.py 8 2`, with 1 % of rows dropped.
- `sched.migrations -m count -e topk` on `migrate.py 200`: 203, 205, 209
  over three runs; 1005 for `migrate.py 1000`.
- `sched.runqlat` on `sleeper.py 100 2`, three runs from `-e raw`: mean
  `wake_to_run_ns` 178, 20 and 2.4 us; mean `run_delay` 174, 16 and 0 us.
- `cargo test --test reference perfsched`: 11 passed.

Reasoned, not shown: every `perf` pair's tolerance. They are set from omn's
spread alone; the tool's side has not run here.

## Not done, and why

- The perf pairs did not run: `/sys/kernel/tracing` is `drwx------ root`
  and `sudo -n` asks for a password.
- The pairs were not run through the runner: stream reference's runner was
  not on `main`. The YAML uses three fields beyond the brief's schema,
  documented at the head of the file: `compare.across: sum`,
  `compare.scale` (`compare.missing: zero` was dropped in review). The `omn` path is written
  `<emit>.<field>` (`summary.count`, `summary.mean`, `topk.total`), the
  brief's example's convention.
- No pair for `strace` argument decoding, `perf sched map`/`replay`,
  `perf sched timehist`'s per-switch lines, `perf lock record/report`
  (lockdep; this kernel has neither `CONFIG_LOCKDEP` nor `CONFIG_LOCK_STAT`):
  omn has no equivalent to compare, each row says so.

## Privileged runs wanted

```console
# perf tracepoints for the user (or run the reference test as root):
sudo mount -o remount,mode=755 /sys/kernel/tracing
# or grant sudo -n for perf alone:
#   <user> ALL=(root) NOPASSWD: /usr/bin/perf
cd /home/moha/repos/omniscient && make && cargo test --test reference
# real captures to replace the documented-layout ones:
W=tests/reference/workloads/perfsched
sudo perf trace -s -o tests/reference/captures/perfsched/perf-trace-s.txt -- python3 $W/syscalls.py 5000 20 10
d=$(mktemp); sudo perf sched record -q -o $d -- python3 $W/sleeper.py 100 2 >/dev/null 2>&1
sudo perf sched latency -i $d > tests/reference/captures/perfsched/perf-sched-latency.txt
sudo perf sched timehist -s -i $d > tests/reference/captures/perfsched/perf-sched-timehist-s.txt
sudo perf lock contention -b -- python3 $W/contend.py 8 2 2> tests/reference/captures/perfsched/perf-lock-contention.txt
```

After replacing a capture, the parser test's expected numbers change with
it.

## Gaps found (each a line of work for omn)

1. `proc.syscall`'s `summary` and `topk` key by task only: a `strace -c`
   table takes one run per syscall (`--filter 'syscall == NAME'`). `syscall`
   as a key (`-k syscall`) would replace `strace -c` and `perf trace -s` in
   one run.
2. A launched program's syscalls include omn's launcher's `PATH` search
   (failed `execve`) and a few calls before exec. Resolve the path before
   fork, or open the window at the successful `execve`.
3. No errors per syscall (`strace -c`'s `errors` column): `proc.syscall`
   does not keep the return value.
4. No per-call record of a syscall: `-m trace` is drained into log2 buckets,
   so `strace -T`'s and `perf trace`'s per-call lines (which call was slow,
   when) have no equivalent.
5. `sched.migrations -m count -e topk` prints no series when it counted
   nothing, rather than a zero: a reader cannot tell "none" from "not
   measured". (Its `rest.total` prints `-0.0` when nothing is left over.)
6. `sched.lock`'s `summary` keys by task; `perf lock contention` groups by
   lock type and caller (`-l`: by address). The rows carry `kind` and
   `flags`; a key on them would serve it.
7. `sched.lock` dropped 1 % of its rows on an 8-thread mmap churn (reported
   as `short`); `proc.syscall` unfiltered dropped 12 of 108 rows once on the
   5000-call workload and none on three later runs.
8. No emit summarises `sched.runqlat`'s `wake_to_run_ns`, which is the
   quantity `perf sched latency` and `runqlat.bt` report; only `-e raw`
   carries it, per drained row.
9. No per-switch event stream joining wakeup, queue wait and run time, which
   `perf sched timehist` prints one line a switch.

## Decision lines

- strace's per-call latency is not a reference for short calls: under
  ptrace a `getppid` takes 16 us against 0.25 us. Only calls the kernel
  makes long (a sleep) are compared on latency.
- `strace -c` is compared with `-w`; without it the time is the call's
  system cpu, not its latency.
- System-wide perf recorders are read by a command name the workload sets
  itself (`omnref-sleep`), not by `python3`, which twelve streams run at
  once.

## Requests to other streams

- **reference**: support (or rename to your schema) `compare.across: sum`,
  and `compare.scale`; resolve `omn: <emit>.<field>`
  against the output whose `emit` matches; run a `privileged: true`
  reference command through `sudo -n` and skip with its refusal. The perf
  sched pairs wrap two perf commands in `sh -c '...' sh`, the workload's
  argv appended as `"$@"`.
- **bpftrace**: `runqlat.bt` and `syscount.bt` overlap `sched.runqlat` and
  `proc.syscall`; the workloads here (`syscalls.py`, `sleeper.py`) give
  exact counts if you want them.

## Review

1. **`sched.migrations` pair vacuous** -- fixed. New workload
   `migrate.py 200` (affinity alternated between two cpus 200 times), key
   `omnref-migrate.migrations`, tolerance `rel: 0.1`; `compare.missing:
   zero` removed from the pair and from the file's schema notes, so an omn
   that prints no series fails. omn measured 203-209 (three runs). The
   reviewer's 8-or-nothing on `sleeper.py` is also what made me stop
   reading that workload for migrations.
2. **Four hand-written perf captures** -- agreed, not fixable here: perf
   cannot reach `/sys/kernel/tracing` unprivileged and `sudo -n perf` asks
   for a password, so no real capture can be made in this session. Done
   instead: the `perf lock contention` parser now ends the table at a
   `===` line, so perf's `=== output for debug ===` block (whose histogram
   has its own `contended: N` line) no longer makes it error, with a test;
   the header is matched on `contended ... caller`, not `contended` alone.
   `docs/replaces/perfsched.md` now says plainly that no perf pair is to be
   trusted until the four captures are replaced. The commands that make
   them are under "Privileged runs wanted"; each parser test's expected
   numbers change with its capture.
3. **`sched.runqlat` / `perf sched latency` compares two quantities** --
   agreed, pair removed rather than reshaped. omn's `summary.mean` is
   `run_delay`; perf's average is wakeup to sched-in, omn's
   `wake_to_run_ns`, which no emit summarises (only `-e raw` rows carry
   it). And a relative band does not hold on alternate runs here: the same
   workload gave a `wake_to_run_ns` mean of 178, 20 and 2.4 us over three
   runs. A pair that either passes anything or fails at random is not kept;
   the replaces row says "no pair" and what would make one (a summary over
   `wake_to_run_ns`, and a same-run comparison the runner does not offer).
   Gap 8 above.
