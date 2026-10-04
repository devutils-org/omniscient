# What omn replaces: the `bpftrace` family

The tools in `/usr/share/bpftrace/tools/` and their bcc namesakes, and the bcc
tools that have no `.bt`. Pairs: `catalog/references/bpftrace.yaml`; parsers:
`tests/reference/bpftrace.rs`; workloads:
`tests/reference/workloads/bpftrace/`.

**Every pair skips on this machine, through the shared runner**: bpftrace
needs root and `sudo -n` is not granted (`cargo test --test reference
pairs_bpftrace`: 0 held, 10 skipped, 0 gaps, 0 known defects, 0 failed).
bcc is not installed. So no row says "same numbers" yet. What is shown is
omn's side: each pair's `omn` argv was run here, its output is captured
under `tests/reference/captures/bpftrace/omn/<id>.json`, and every compare's
path or parser is evaluated on it by
`bpftrace::tests::every_omn_side_resolves_on_its_capture`. *omn shown* below
is that number against what the workload fixes; *tool pending* is what the
privileged run settles (`docs/fixes/w23-bpftrace.md`, "Privileged runs
wanted").

| Tool and use | omn equivalent | Covered | Same numbers | Missing |
|---|---|---|---|---|
| `syscount.bt`: syscalls per process | `omn trace proc.syscall -m count -e summary --filter 'comm == "X"'` | per process and per syscall name; one process at a time per request | skipped (root); omn shown: 21216 calls for 20000 `getppid` and python3's startup; pair `syscount` holds it within 5%, tool pending | a system-wide top-N by process in one request (`-k comm`); a comm filter that follows a rename (see Missing below) |
| `opensnoop.bt`: who opens what | `omn trace proc.syscall --filter 'syscall == "openat"'` | the count and latency of opens; not the path, nor the fd returned | skipped (root); omn shown: 623 openat for 500 opens plus startup; pair `opensnoop`, within 5%, tool pending | the opened path and the returned fd as attributes; `open`/`openat2` folded into one "open" selector |
| `killsnoop.bt`: who signals whom | `omn trace proc.signals -m count --filter 'sig == 12'` | the count of signals per receiving task, by number | skipped (root); omn shown: exactly 50 SIGUSR2; pair `killsnoop`, exact, tool pending | the sender's comm and pid, the kill(2) return value |
| `execsnoop.bt`: programs executed | `omn trace proc.exec --system --filter 'comm == "true"'` | every exec, system-wide or per cgroup | skipped (root); omn shown: exactly 20 `/bin/true`, one run beside the tool; pair `execsnoop`, exact, tool pending | `proc.exec` per launch (the programs a launched command executes); argv as an attribute |
| `bitesize.bt`: block I/O size per process | `omn trace io.size -e hist:log2 --filter 'comm == "X"'` | the same histogram, per device instead of per process name | skipped (root); omn shown: 504 requests in [4K, 8K) for 500 O_DIRECT 4 KiB reads; pair `bitesize` compares that bucket within 5%, tool pending | a `comm` correlation key, to split the histogram per process as the tool does |
| `biosnoop.bt`: one line per block request | `omn trace io.latency -m trace -e raw` | request, device, latency, issuer | skipped (root); omn shown: 508 completed requests by the workload; pair `biosnoop`, within 5%, tool pending | -- |
| `biolatency.bt`: block latency histogram | `omn trace io.latency -e hist:log2` | the same histogram, per device | skipped (root); omn shown: median in [32, 64) us over 508 requests; pair `biolatency`, the median within one log2 bucket, tool pending | -- |
| `runqlat.bt`: run queue latency | `omn trace sched.runqlat -e hist:log2` | the same histogram, per task, cgroup or CPU | skipped (root); omn shown: 26406 waits, median in [64, 128) us under two spinners per CPU; pair `runqlat`, one log2 bucket, tool pending | -- |
| `runqlen.bt`: run queue length | none | 0 | no pair: omn has no run queue length source | `sched.runqlen`: sampled `nr_running` per CPU (perf sampling or a BPF timer), an lhist |
| `tcpconnect.bt`: active connections | `omn trace net.tcp.connect -m trace` | every connect with its latency and outcome | skipped (root); omn shown: exactly 10; pair `tcpconnect`, exact, tool pending | the addresses and ports as attributes on the summary |
| `tcpaccept.bt`: passive connections | none | 0 | no pair: no accept event in omn (`net.backlog` counts queue drops) | `net.tcp.accept`: inet_csk_accept return with addresses and backlog length |
| `tcplife.bt`: session duration and volume | `omn trace net.tcp.lifetime --system --filter 'comm == "X"'` | sessions with duration and bytes, system-wide only | skipped (root); omn shown: exactly 20 sockets for 10 local connections, one run beside the tool; pair `tcplife`, exact, tool pending | `net.tcp.lifetime` per launch or pid |
| `tcpretrans.bt`: retransmits | `omn trace net.tcp.retransmit` | per socket and task | no pair: no deterministic retransmit without a qdisc (`tc netem`) or a firewall drop rule, both root | -- (a privileged pair with `tc qdisc add dev lo root netem loss 10%`) |
| `tcpdrop.bt`: TCP drops with kernel stack | `omn trace net.drops` | drops with reason, system-wide | no pair: tcpdrop.bt keeps only skbs with a socket and a reason above NOT_SPECIFIED, which no unprivileged workload produces deterministically | the kernel stack of the drop; a `protocol == tcp` view keyed by socket addresses |
| `vfsstat.bt`: VFS calls per second | `omn trace io.fs -m count` (regular files only) | read/write/open/fsync on regular files | no pair: the scopes differ by design -- vfsstat counts every vfs_read* system-wide, pipes and sockets included; io.fs keeps regular files | a count of every VFS call by operation, any file type, per second (`-e rate` with `op` as a key) |
| `writeback.bt`: writeback events | `omn trace io.writeback` | writeback per device, file and cgroup | no pair: when writeback runs and in how many pages is the kernel's choice, not a workload's; two runs do not see the same events | the reason (`periodic`, `background`, `sync`) as an attribute |
| `swapin.bt`: swap-ins per process | `omn trace mem.swap` (counters) | swap-in volume, system or cgroup, not per process | no pair: swapping needs memory pressure, which a test workload cannot cause without a cgroup limit set as root | per-process swap-in (`swap_read_folio`), keyed by task |
| `oomkill.bt`: OOM kills | none | 0 | no pair: no OOM event in omn | `mem.oom`: the oom_kill_process victim, trigger and cgroup |
| `offcputime` (bcc): off-CPU time by stack | `omn trace sched.offcpu -e stack` | blocked time with the blocking stack | no pair: bcc is not installed; omn's `-e summary`/`hist` of `sched.offcpu` on a launched TCP workload came out empty, not yet explained | folded output (`-e stack --format folded`) to compare with `offcputime -f` |
| `wakeuptime` (bcc): who wakes whom, by stack | `omn trace sched.wakeup` | the wakeup chain as counts | no pair: bcc is not installed, and wakeuptime sums blocked time by waker stack where sched.wakeup counts wakeups | blocked time attributed to the waker's stack |
| `cachestat` (bcc): page cache hit ratio | `omn trace io.pagecache -m count` | misses against lookups, per file and task | no pair: bcc is not installed | the per-second HITS/MISSES/DIRTIES columns system-wide |
| `ext4slower` (bcc): slow ext4 operations | `omn trace io.fs --filter 'fs == "ext4" && ...'` | VFS latency on any filesystem | no pair: bcc is not installed, and this machine's filesystems are btrfs | a latency threshold filter on io.fs, which the filter grammar may already express (`lat_ns > ...`), unverified |
| `memleak` (bcc): outstanding allocations | `omn trace mem.heap.live` | stream heap owns this tool | no pair: stream heap's, see `docs/replaces/heap.md` | -- |
| `hardirqs` (bcc): time in hard IRQs | `omn trace sched.irq --system` | per CPU, with irq line and handler name | no pair: bcc is not installed; and `--filter 'kind == "hard"'` selects nothing (see Missing below) | a working `kind` filter; a key by handler name, as the tool totals per name |
| `softirqs` (bcc): time in soft IRQs | `omn trace sched.irq --system --filter 'name == "TIMER"'` | per CPU, per vector by name | no pair: bcc is not installed | a key by vector name; `kind == "soft"` (same defect) |

## Missing, across the family

- **A comm filter on proc.syscall reads the comm an aggregate first saw.** A
  task that renames itself (prctl `PR_SET_NAME`) after a syscall already ran
  keeps its old comm on that syscall's aggregate: `--filter 'comm == "new"'`
  then drops every later call of it. Shown: a workload that renames itself to
  `omnref-open` and opens a file 500 times gave 1 `openat` under that filter;
  raw events showed 622 `openat` under `python3` and 1 under `omnref-open`.
  bpftrace evaluates `comm` per event. The workloads here name themselves at
  exec to stay clear of it.
- **`sched.irq --filter 'kind == "hard"'` (or `"soft"`) returns no series**,
  in count and trace mode alike, while raw events carry `kind: hard` and
  `kind: soft`; `name == "TIMER"` and `irq >= 0` do filter. A filter that
  silently selects nothing reads as "no interrupts".
- **Per-launch targets** for `proc.exec` and `net.tcp.lifetime`: both are
  system- or cgroup-only, so a pair has to run omn `--system` beside the
  workload.
- **Keys by comm**: the bpftrace tools aggregate by process name; omn keys by
  task, and selects a name only through a filter, one per request.
