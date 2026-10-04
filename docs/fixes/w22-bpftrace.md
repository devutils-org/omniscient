# The twenty-second wave, stream bpftrace

The tools in `/usr/share/bpftrace/tools/` and their bcc namesakes, plus the
bcc tools with no `.bt`, against omn. bpftrace 0.27 is installed here but
needs root, and `sudo -n` is not granted; bcc is not installed. So every pair
skips on this machine, and what is shown is omn's side against what each
workload fixes.

## What landed

- `catalog/references/bpftrace.yaml`: ten pairs, all `privileged: true`,
  `requires: [bpftrace]`: syscount, opensnoop, killsnoop, execsnoop, bitesize,
  biosnoop, biolatency, runqlat, tcpconnect, tcplife. The header documents
  the omn paths these pairs use and the one family field (`alongside`).
- `tests/reference/bpftrace.rs`: nine parsers -- `bpftrace_map` (any
  `@map[key]: N` output, summed over interval prints, composite keys also
  summed by their first component), `bpftrace_hist` and
  `bpftrace_hist_usecs` (any `hist()`/`lhist()`, as `<map>/count`,
  `<map>/p50`, `<map>/<low>`; the `usecs` one scales bounds to ns),
  `execsnoop`, `killsnoop`, `opensnoop`, `biosnoop`, `tcpconnect`, `tcplife`
  -- and 16 unit tests on the captures.
- `tests/reference/captures/bpftrace/`: 13 captures (the ten pairs' tools plus
  `runqlen`, `vfsstat`, `swapin`, whose output the generic parsers also read).
- `tests/reference/workloads/bpftrace/`: `act.py` (one deterministic action N
  times: getppid, open, signal, connect, refused, read with O_DIRECT, vfsread,
  spin), `omnref` (a symlink to python3, so the workload's comm is `omnref`
  from exec), `execs.sh` (N execs of `/bin/true`), and `under.sh` (runs a
  `.bt` tool with the workload as its `-c` child).
- `docs/replaces/bpftrace.md`: a row for each of the 25 tools, with a pair or
  the reason there is none, and the gaps across the family.

## Shown versus reasoned

Shown, on this machine, omn's side of each pair on its workload
(`target/debug/omn` with `cap_bpf,cap_perfmon`):

| Pair | omn command (abridged) | omn value | the workload fixes |
|---|---|---|---|
| syscount | `proc.syscall -m count --filter 'comm == "omnref"'` on 20000 getppid | 21217 calls; 20000 exactly with `syscall == "getppid"` | 20000 + startup |
| opensnoop | `proc.syscall --filter 'syscall == "openat" && comm == "omnref"'` on 500 opens | 623 | 500 + startup |
| killsnoop | `proc.signals -m count --filter 'sig == 12'` on 50 kills | `last` 50 | 50 |
| execsnoop | `proc.exec --system -d 3s --filter 'comm == "true"'`, 20 execs beside it | 20 | 20 |
| bitesize | `io.size -e hist:log2 --filter 'comm == "omnref"'` on 500 O_DIRECT 4 KiB reads | 508; 501 in [4K, 8K) | 500 + setup write and fsync |
| biosnoop/biolatency | `io.latency -e hist:log2 --filter 'comm == "omnref"'` | 508; median in [4, 8) us | 500 + setup |
| runqlat | `sched.runqlat -e hist:log2 --filter 'comm == "omnref"'`, 2 spinners per CPU | 16747 waits, median in [2, 4) ms | -- |
| tcpconnect | `net.tcp.connect -m trace` on 10 connections | 10 | 10 |
| tcplife | `net.tcp.lifetime --system -d 3s --filter 'comm == "omnref"'`, beside 10 connections | 20 | 20 sockets |

Shown, the two omn defects below (commands in "Gaps").

Reasoned, not shown: every tool-side number. The captures are written from
each `.bt` script's own `printf` formats and bpftrace's map and histogram
layout, with the numbers omn measured; they are not the tool's output on this
machine. The privileged run replaces them. Also reasoned: that bpftrace's
`-c` splits its string on spaces (no workload word here has one), and that
the tolerances (5% where two runs differ by python3's startup, which runs as
root under bpftrace; one log2 bucket for medians) hold.

## Not done, and why

- **No pair ran through the runner**: stream reference's runner was not on
  `main` when this stream finished; `cargo test --test reference` ran the
  parser tests only.
- **No pair for** runqlen, tcpaccept, oomkill (no omn source); tcpretrans,
  tcpdrop, writeback, swapin (no deterministic event without root: a netem
  qdisc, a firewall rule, a memory-limited cgroup; writeback is the kernel's
  timing); vfsstat (the scopes differ by design). Each row says so.
- **No bcc pair or parser**: bcc is not installed, so there is no capture to
  test a parser on; writing one from memory of its layout was judged worse
  than filing it. memleak is stream heap's row.
- **sched.offcpu** came out empty (`-e summary` and `-e hist`) on the launched
  TCP workload; not investigated, so not filed as a defect.

## Gaps in omn, each a line of work (filed, not fixed)

1. **proc.syscall's `comm` filter reads the comm an aggregate first saw.** A
   task that renames itself (prctl `PR_SET_NAME`) keeps its old comm on every
   syscall it had already made once, and a filter on the new name drops
   those calls. Shown with an earlier `act.py` that renamed itself to
   `omnref-open` before 500 opens:
   `omn trace proc.syscall -m count -e summary --filter 'syscall == "openat" && comm == "omnref-open"' -- python3 act.py open 500`
   gave `count` 1; the same run with `-m trace -e raw` and no filter showed
   622 `openat` under `python3` and 1 under `omnref-open`.
2. **`sched.irq --filter 'kind == "hard"'` (or `"soft"`) returns no series**,
   in count and trace mode, while raw events carry `kind`; `name == "TIMER"`
   and `irq >= 0` filter as expected. Shown with
   `omn trace sched.irq -m trace -e summary --system -d 2s --filter 'kind == "hard"'`
   (0 series) against `-e raw` (133 hard, 1835 soft events in 1 s). It
   breaks "an invalid combination is refused with the reason, never degraded
   silently".
3. `proc.exec` and `net.tcp.lifetime` per launch (both are system/cgroup only).
4. A `comm` correlation key (`-k comm`) on proc.syscall, io.size,
   io.latency: the tools aggregate by process name, omn by task.
5. New sources: `sched.runqlen` (sampled run queue length, lhist),
   `net.tcp.accept` (accept with addresses and backlog), `mem.oom` (victim,
   trigger, cgroup), per-process swap-in.
6. Attributes: opened path and returned fd on proc.syscall's openat; the
   sender on proc.signals; the writeback reason on io.writeback; the kernel
   stack on net.drops.
7. A count of every VFS call by operation, any file type, per second (what
   vfsstat reports; io.fs keeps regular files).

## Privileged runs and installs wanted

```console
# run the pairs (bpftrace needs root):
sudo -E cargo test --test reference -- --nocapture bpftrace
# or grant sudo -n for bpftrace alone, which the runner uses:
#   <user> ALL=(root) NOPASSWD: /usr/bin/bpftrace
# real captures, to replace the written ones (each from the repo root):
W=tests/reference/workloads/bpftrace; C=tests/reference/captures/bpftrace
for t in syscount opensnoop killsnoop bitesize biosnoop biolatency tcpconnect; do
  a=$(case $t in syscount) echo "getppid 20000";; opensnoop) echo "open 500";;
      killsnoop) echo "signal 50";; tcpconnect) echo "connect 10";;
      *) echo "read 500";; esac)
  sudo sh $W/under.sh /usr/share/bpftrace/tools/$t.bt -- $PWD/$W/omnref $PWD/$W/act.py $a > $C/$t.txt
done
sudo sh $W/under.sh /usr/share/bpftrace/tools/runqlat.bt -- $PWD/$W/omnref $PWD/$W/act.py spin 2 > $C/runqlat.txt
sudo sh $W/under.sh /usr/share/bpftrace/tools/tcplife.bt -- $PWD/$W/omnref $PWD/$W/act.py connect 10 > $C/tcplife.txt
sudo sh $W/under.sh /usr/share/bpftrace/tools/execsnoop.bt -- sh $PWD/$W/execs.sh 20 > $C/execsnoop.txt
# then adjust the unit tests' expected numbers to the real captures.
# bcc, for the rows that have none:
sudo pacman -S bcc-tools python-bcc
```

## Decision lines

- The bpftrace pairs compare **two runs**, never one: bpftrace launches its
  `-c` child itself and omn cannot attach to a process not yet created. Only
  quantities the workload fixes are compared exactly; latencies are medians
  held to one log2 bucket.
- Workloads name themselves **at exec** (a symlink named `omnref`), not by
  prctl, so the comm is the same from the first event (and gap 1 does not
  bite the pairs).
- The parsers scale units (`bpftrace_hist_usecs` to ns) rather than the pair.

## Requests to other streams

- **reference** (the runner): read `compare.omn` as `summary.count`,
  `summary.last` (sums over series, as other families use), and
  `hist.count` / `hist.p50` (bucket counts summed; the lower bound of the
  median bucket over merged series) -- documented in the pair file's header.
  Support `alongside: true`: omn runs with its own `--system -d` and no
  program, and the runner starts the workload beside it (execsnoop,
  tcplife). Resolve a relative workload `argv[0]` from the repository root
  (`tests/reference/workloads/bpftrace/omnref`). Skip privileged pairs before
  running omn, with the `sudo -n` reason.
- **src owners (after the wave)**: gaps 1 and 2 are defects, not features.

## Tests run

- `cargo test --test reference`: 72 passed, 1 ignored, 0 failed (16 of them
  this family's), after merging `main`.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
- `cargo fmt --check`: clean.

## Review

Both findings were right, and both are fixed. **Where the fix landed is the
odd part**: see "Fix round: where the commits are" below.

1. **The median tolerance was too tight, and asymmetric.** The two grids
   differ: bpftrace's buckets are powers of two in microseconds (the
   reference p50 is r = 1000 * 2^k ns), omn's powers of two in nanoseconds.
   The same latency puts omn's p50 at 2^(k+9) = 0.512 r or
   2^(k+10) = 1.024 r. One bucket above that is 2^(k+11) = 2.048 r, off by
   1.048 r, which `rel: 1.0` refused (biolatency 8192 against 4000, runqlat
   4,194,304 against 2,048,000). Both pairs are now `rel: 1.1`, which admits
   2.048 r and refuses 2^(k+12) = 4.096 r. The pair file's header has the
   arithmetic. Not fixed: below r a relative tolerance of 1 or more bounds
   nothing, so a median of 0 would pass. A tolerance on log2 bucket indices
   would be symmetric. That is a request to the runner (below), not something
   this family can add alone.
2. **The captures were omn's numbers in tool-named files.** All 13 are now
   `<tool>.format.txt`, and the numbers omn measured are replaced by
   synthetic ones that agree with no tool and no omn run: syscount 7313/7001,
   bitesize 308/301, biolatency 333 with p50 8000 ns, runqlat 10747 with p50
   1,024,000 ns, execsnoop 6 `/bin/true`, killsnoop 12 SIGUSR2, tcpconnect 3.
   The test helper is now `fixture()`, and its doc comment says these files
   check the parsers, never a pair. The real output from the privileged run
   goes beside them as `<tool>.txt`, and the loop under "Privileged runs"
   should write there. "Shown versus reasoned" above already said the tool
   side was reasoned. That still holds, and the files now say it too.

### Fix round: where the commits are

While this round ran its baseline test, it used `git stash` and
`git stash pop` on the tree. The stash list is shared by every worktree of
the repository, and stream reference stashed and popped at the same time. The
two stashes swapped places:

- **This round's fix** (the yaml, `tests/reference/bpftrace.rs`, the 13
  renamed fixtures) was popped into `omniscient-w22-reference`. It is
  committed there in **c2194ab** on `feat/w22-reference`, labelled as a
  reference fix. Merging that branch brings it into `main`. It is also in
  the dangling stash commit `0a9b1a1`.
- **Stream reference's runner fix** (`tests/reference/runner.rs`,
  `tests/scenario_cpu.rs`, `tests/scenario_proc.rs`, the work c2194ab's
  message describes) was popped into this worktree. It is left there
  uncommitted and untouched, also saved as the dangling stash commit
  `c72ff98`. Restoring this tree from `0a9b1a1` and dropping those three
  files was refused by the session's permissions, so the coordinator has
  to sort it out.
- Lesson: never run `git stash` in a worktree while other streams run. Use
  `git worktree add` or a commit instead.

### Requests from this round

- **reference** (the runner): a tolerance on log2 bucket indices
  (`{log2: 1}`: |log2(omn) - log2(reference)| <= 1), so a median is bounded
  both ways.

## Tests run, fix round

- `cargo test --test reference`, with the fix in place (before the swap):
  97 passed, 9 failed, 1 ignored. All 16 bpftrace parser tests and
  `pairs_bpftrace` passed. The 9 failures are other families' pairs against
  the runner just merged from `main` (perfrecord, io, top, power, heap,
  sysstat, perfstat, `every_pair_is_well_formed`,
  `every_row_names_a_pair_or_why_none`). `main` without the fix gives the
  same 97/9/1.
- `cargo fmt --all -- --check` and
  `cargo clippy --workspace --all-targets -- -D warnings`: clean.
