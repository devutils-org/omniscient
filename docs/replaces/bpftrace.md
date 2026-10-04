# What omn replaces: the `bpftrace` family

The tools in `/usr/share/bpftrace/tools/` and their bcc namesakes, and the bcc
tools that have no `.bt`. Pairs: `catalog/references/bpftrace.yaml`; parsers:
`tests/reference/bpftrace.rs`; workloads:
`tests/reference/workloads/bpftrace/`.

**Every pair skips on this machine**: bpftrace needs root and `sudo -n` is not
granted, and bcc is not installed. "Same numbers" therefore says what omn
measured on the pair's workload against what the workload fixes (a number of
calls, signals, connections), and the tool's side is still to be run
(`docs/fixes/w22-bpftrace.md`, "Privileged runs wanted"). Where a cell says
*omn shown*, the number is omn's on this machine; *tool pending* is what the
privileged run will settle.

| Tool and use | omn equivalent | Covered | Same numbers | Missing |
|---|---|---|---|---|
| `syscount.bt`: syscalls per process | `omn trace proc.syscall -m count -e summary --filter 'comm == "X"'` | per process and per syscall name; one process at a time per request | omn shown: 20000 `getppid` exactly with `syscall == "getppid"`; within 5% against the tool (pair `syscount.bt`), tool pending | a system-wide top-N by process in one request (`-k comm`); a comm filter that follows a rename (see Missing below) |
| `opensnoop.bt`: who opens what | `omn trace proc.syscall --filter 'syscall == "openat"'` | the count and latency of opens; not the path, nor the fd returned | omn shown: 623 openat for 500 opens plus startup; within 5% (pair `opensnoop.bt`), tool pending | the opened path and the returned fd as attributes; `open`/`openat2` folded into one "open" selector |
| `killsnoop.bt`: who signals whom | `omn trace proc.signals -m count --filter 'sig == 12'` | the count of signals per receiving task, by number | omn shown: exactly 50 SIGUSR2; exact (pair `killsnoop.bt`), tool pending | the sender's comm and pid, the kill(2) return value |
| `execsnoop.bt`: programs executed | `omn trace proc.exec --system --filter 'comm == "true"'` | every exec, system-wide or per cgroup | omn shown: exactly 20 `/bin/true`; exact (pair `execsnoop.bt`), tool pending | `proc.exec` per launch (the programs a launched command executes); argv as an attribute |
| `bitesize.bt`: block I/O size per process | `omn trace io.size -e hist:log2 --filter 'comm == "X"'` | the same histogram, per device instead of per process name | omn shown: 508 requests, 501 in [4K, 8K) for 500 O_DIRECT 4 KiB reads; within 5% (pair `bitesize.bt`), tool pending | a `comm` correlation key, to split the histogram per process as the tool does |
| `biosnoop.bt`: one line per block request | `omn trace io.latency -m trace -e raw` | request, device, latency, issuer | omn shown: 508 completed requests by the workload; within 5% (pair `biosnoop.bt`), tool pending | -- |
| `biolatency.bt`: block latency histogram | `omn trace io.latency -e hist:log2` | the same histogram, per device | omn shown: median in [4, 8) us; median within one log2 bucket (pair `biolatency.bt`), tool pending | -- |
| `runqlat.bt`: run queue latency | `omn trace sched.runqlat -e hist:log2` | the same histogram, per task, cgroup or CPU | omn shown: 16747 waits, median in [2, 4) ms under two spinners per CPU; one log2 bucket (pair `runqlat.bt`), tool pending | -- |
| `runqlen.bt`: run queue length | none | 0 | no pair: omn has no run queue length source | `sched.runqlen`: sampled `nr_running` per CPU (perf sampling or a BPF timer), an lhist |
| `tcpconnect.bt`: active connections | `omn trace net.tcp.connect -m trace` | every connect with its latency and outcome | omn shown: exactly 10; exact (pair `tcpconnect.bt`), tool pending | the addresses and ports as attributes on the summary |
| `tcpaccept.bt`: passive connections | none | 0 | no pair: no accept event in omn (`net.backlog` counts queue drops) | `net.tcp.accept`: inet_csk_accept return with addresses and backlog length |
| `tcplife.bt`: session duration and volume | `omn trace net.tcp.lifetime --system --filter 'comm == "X"'` | sessions with duration and bytes, system-wide only | omn shown: exactly 20 sockets for 10 local connections; exact (pair `tcplife.bt`), tool pending | `net.tcp.lifetime` per launch or pid |
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
| `memleak` (bcc): outstanding allocations | `omn trace mem.heap.live` | stream heap owns this row (BRIEF.md) | see `docs/replaces/heap.md` | -- |
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
