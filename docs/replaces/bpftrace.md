# What omn replaces: the `bpftrace` family

The tools in `/usr/share/bpftrace/tools/` and their bcc namesakes, and the bcc
tools that have no `.bt`. Pairs: `catalog/references/bpftrace.yaml`; parsers:
`tests/reference/bpftrace.rs`; workloads:
`tests/reference/workloads/bpftrace/`.

**Through the shared runner, with root** (`/etc/sudoers.d` granting
`bpftrace` without a password; `cargo test --test reference pairs_bpftrace`,
2026-10-04): 9 held, 0 skipped, 1 gap, 0 known defects, 0 failed. Each
wrapper runs unprivileged and only `bpftrace` goes through `sudo -n`
(`reference.sudo`). The numbers below are one quiet run; each pair's `omn`
side is also checked on its capture under
`tests/reference/captures/bpftrace/omn/<id>.json`
(`bpftrace::tests::every_omn_side_resolves_on_its_capture`).

| Tool and use | omn equivalent | Covered | Same numbers | Missing |
|---|---|---|---|---|
| `syscount.bt`: syscalls per process | `omn trace proc.syscall -m count -e summary --filter 'comm == "X"'` | per process and per syscall name; one process at a time per request | yes: 21220 against 21214 calls, pair `syscount` | a system-wide top-N by process in one request (`-k comm`); a comm filter that follows a rename (see Missing below) |
| `opensnoop.bt`: who opens what | `omn trace proc.syscall --filter 'syscall == "openat"'` | the count and latency of opens; not the path, nor the fd returned | yes: 623 against 624 opens, pair `opensnoop`; once under the wave's load opensnoop.bt printed 427 (reasoned, not shown: its output buffer short, that run's stderr was not kept), so the parser now refuses an output where bpftrace reports `Lost N events` | the opened path and the returned fd as attributes; `open`/`openat2` folded into one "open" selector |
| `killsnoop.bt`: who signals whom | `omn trace proc.signals -m count --filter 'sig == 12'` | the count of signals per receiving task, by number | yes, exact: 50 SIGUSR2, pair `killsnoop` | the sender's comm and pid, the kill(2) return value |
| `execsnoop.bt`: programs executed | `omn trace proc.exec --system --filter 'comm == "true"'` | every exec, system-wide or per cgroup | yes, exact: 20 `/bin/true`, one run beside the tool, pair `execsnoop` | `proc.exec` per launch (the programs a launched command executes); argv as an attribute |
| `bitesize.bt`: block I/O size per process | `omn trace io.size -e hist:log2 --filter 'comm == "X"'` | the same histogram, per device instead of per process name | yes, exact: 502 requests in [4K, 8K), pair `bitesize` | a `comm` correlation key, to split the histogram per process as the tool does |
| `biosnoop.bt`: one line per block request | `omn trace io.latency -m trace -e raw` | request, device, latency, issuer | yes: 508 against 509 requests, pair `biosnoop` | -- |
| `biolatency.bt`: block latency histogram | `omn trace io.latency -e hist:log2` | the same histogram, per device | yes: the median one log2 bucket apart (4096 ns against 4000), pair `biolatency` | -- |
| `runqlat.bt`: run queue latency | `omn trace sched.runqlat --system -e hist:log2 -- prog` | the same histogram, the machine's for the program's life (or per task, cgroup or CPU) | yes, within one log2 bucket: medians 2097152 ns against 2048000 (buckets 21 and 20) twice at the twenty-fourth wave's load, pair `runqlat`; on a quiet machine the twenty-third wave's gap read 2.1 ms (the program alone) against 128 us (the machine), which `--system` beside the program removes | |
| `runqlen.bt`: run queue length | none | 0 | no pair: omn has no run queue length source | `sched.runqlen`: sampled `nr_running` per CPU (perf sampling or a BPF timer), an lhist |
| `tcpconnect.bt`: active connections | `omn trace net.tcp.connect -m trace` | every connect with its latency and outcome | yes, exact: 10 connections, pair `tcpconnect` | the addresses and ports as attributes on the summary |
| `tcpaccept.bt`: passive connections | none | 0 | no pair: no accept event in omn (`net.backlog` counts queue drops) | `net.tcp.accept`: inet_csk_accept return with addresses and backlog length |
| `tcplife.bt`: session duration and volume | `omn trace net.tcp.lifetime --system --filter 'comm == "X"'` | sessions with duration and bytes, system-wide only | yes, exact: 20 sockets, pair `tcplife` | `net.tcp.lifetime` per launch or pid |
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
