# What omn replaces

`omn` exists to be one tool instead of twenty. This is the proof, tool by
tool: for each tool and each of its common uses, what `omn` serves of it,
whether it gives **the same numbers on the same workload** within a declared
tolerance, and what is missing for it to replace the tool fully.

Each family has its own table, one row per tool and use; each row is backed
by a **pair** in `catalog/references/<family>.yaml` that
`tests/reference.rs` runs -- `omn` and the tool on one workload, the tool's
output parsed, held to the pair's tolerance -- or says `no pair` and why.
The schema of a pair, how it runs and when it skips:
[`catalog/references/README.md`](../catalog/references/README.md).

```console
make && cargo test --test reference -- --nocapture   # every family
```

## The families

| Family | Tools | Table |
|---|---|---|
| perfstat | `perf stat` | [replaces/perfstat.md](replaces/perfstat.md) |
| perfrecord | `perf record`/`report`, `perf mem`, `perf c2c`, `perf top` | [replaces/perfrecord.md](replaces/perfrecord.md) |
| perfsched | `perf sched`, `perf lock`, `perf trace -s`, `strace -c`/`-T` | [replaces/perfsched.md](replaces/perfsched.md) |
| bpftrace | `/usr/share/bpftrace/tools/` and the bcc tools | [replaces/bpftrace.md](replaces/bpftrace.md) |
| sysstat | `vmstat`, `free`, `numastat`, `slabtop`, load, PSI; `iostat`, `mpstat`, `pidstat`, `sar` | [replaces/sysstat.md](replaces/sysstat.md) |
| top | `top`, `htop`, `btop`, `pmap`, `smem` | [replaces/top.md](replaces/top.md) |
| heap | `massif`, `dhat`, `heaptrack`, `memleak` | [replaces/heap.md](replaces/heap.md) |
| net | `ss`, `nstat`, `ip -s link`, `tcpdump`'s counts | [replaces/net.md](replaces/net.md) |
| power | `turbostat`, `cpupower monitor`, `powertop`, RAPL | [replaces/power.md](replaces/power.md) |
| gpu | `nvidia-smi`, `nsys`, `ncu` | [replaces/gpu.md](replaces/gpu.md) |
| io | `fio`, `iostat`'s columns from `/proc/diskstats` | [replaces/io.md](replaces/io.md) |

## Across families

From the eleven tables at the twenty-fourth wave's merge. **Every verdict
here is the shared runner's**: each family's pairs run through `cargo test
--test reference`, and the tally below is the coordinator's run alone on a
quiet machine, one family after another (`--test-threads=1`), at `a5f0acb`
(2026-10-05; `docs/open-findings.md`, "The twenty-fourth wave, coordinator,
after the merge"), sysstat read after `367ad20`.

| Family | Pairs | Held | Skipped | Gaps | Known defects | Failed |
|---|---|---|---|---|---|---|
| [perfstat](replaces/perfstat.md) | 13 | 12 | 0 | 0 | 0 | 1 |
| [perfrecord](replaces/perfrecord.md) | 5 | 5 | 0 | 0 | 0 | 0 |
| [perfsched](replaces/perfsched.md) | 9 | 8 | 0 | 0 | 0 | 1 |
| [bpftrace](replaces/bpftrace.md) | 10 | 10 | 0 | 0 | 0 | 0 |
| [sysstat](replaces/sysstat.md) | 13 | 13 | 0 | 0 | 0 | 0 |
| [top](replaces/top.md) | 11 | 11 | 0 | 0 | 0 | 0 |
| [heap](replaces/heap.md) | 9 | 8 | 0 | 1 | 0 | 0 |
| [net](replaces/net.md) | 8 | 7 | 1 | 0 | 0 | 0 |
| [power](replaces/power.md) | 7 | 7 | 0 | 0 | 0 | 0 |
| [gpu](replaces/gpu.md) | 21 | 21 | 0 | 0 | 0 | 0 |
| [io](replaces/io.md) | 9 | 9 | 0 | 0 | 0 | 0 |
| **all** | **115** | **111** | **1** | **1** | **0** | **2** |

What moved since the twenty-third wave: **no pair skips on `sudo -n`**
(cpupower granted, the runner asking per tool); `runqlat` is no longer a gap
(it runs with `--system` beside the program, the bounded window stream run
landed); `top-cpus` holds (its bias was `cpu.util`'s leading event, fixed);
`pidstat -w` holds (its `known_defect` was the counted window, fixed); io
gained `fio-clat-p50` and `iostat-w-per-s`. The one skip is
`tcpdump-captured`, run without `make CAPTURE=1` (it held with it in stream
reference's run, 1810 against 1810). The gap is `memleak`, which counts
mmap'd memory `mem.heap.live` does not. The two failures are filed:
`cpu.cycles.system` (omn's own start and exit dominate the difference on an
idle machine; the twenty-third wave's build fails the same way by hand) and
`perf-trace-count` (`perf trace -s` reports lost events on every run here,
and the parser refuses a lossy output).

### Replaced fully

**No tool as a whole.** These common uses are covered, and a pair holds
through the runner:

- `perf stat -e <event> -- prog` for instructions, cycles, IPC,
  branch-misses, cache-misses, page-faults, context-switches,
  cpu-migrations and task-clock, per process, system-wide (`-a`) and per
  cgroup (`-G`); `perf stat -p PID`'s raw count
  ([perfstat](replaces/perfstat.md)).
- `perf record` + `perf report --sort dso,sym` self shares, `perf top -p`,
  `perf mem`'s mean load latency, `perf c2c`'s hottest line
  ([perfrecord](replaces/perfrecord.md)).
- `perf sched`, `perf lock contention`, `strace -c -w` for one syscall's
  count and wall time, `strace -T`'s mean on a long call
  ([perfsched](replaces/perfsched.md)).
- syscount, opensnoop, killsnoop, execsnoop, bitesize, biosnoop,
  biolatency, runqlat, tcpconnect, tcplife ([bpftrace](replaces/bpftrace.md)).
- `vmstat`'s busy share (and `si`/`so`, at zero), `/proc/pressure/*`,
  `mpstat`, `iostat -c`, `sar -u` live, `numastat -p`, `pidstat -r` and
  `pidstat -w` ([sysstat](replaces/sysstat.md)).
- `top -b -p`'s RES, TIME+ and `%CPU`, and `%Cpu(s)`; `pmap -x` and `-XX`
  totals (RSS, PSS, USS); `smem -P`'s RSS, PSS and USS
  ([top](replaces/top.md)).
- dhat's total bytes and blocks, `t-gmax` and `t-end`; massif's peak;
  heaptrack's calls, peak and leaked bytes ([heap](replaces/heap.md)).
- `ip -s link`'s bytes and packets; `nstat`'s listen overflows, listen
  drops and `TcpRetransSegs`; `ss -ti`'s rtt; tcpdump's filtered count
  ([net](replaces/net.md)).
- Package energy against `perf stat -a -e power/energy-pkg/` and
  `turbostat`; `turbostat`'s busy frequency; `cpupower monitor -m
  Idle_Stats` and `-m Mperf`; the RAPL files; `sensors`
  ([power](replaces/power.md)).
- nsys's launches, copies and driver-call counts per kernel and call, kernel
  and copy device time; ncu's instruction, block, L1 and DRAM counters;
  nvidia-smi's power, shader clock, utilisation, temperature and
  per-process VRAM, query and `dmon` ([gpu](replaces/gpu.md)).
- fio's completed reads, bytes and operations and its clat median;
  `iostat -dx`'s `r/s`, `rkB/s`, `r_await` and `w/s` ([io](replaces/io.md)).

### Replaced in part

| Tool | What `omn` serves, and agrees on | What it lacks (the gap, filed) |
|---|---|---|
| `perf stat` | the rows above; `branches`, several events on one launch, `-r N` (0.018 % from `perf stat -r 5`), task-clock, `-a -- prog`, `--pid` ending with its process, all shown by hand | pairs for the new shapes; `cpu.cycles.system` red on a quiet machine (filed) |
| `perf record`/`report`, `perf top` | self shares by function and object; a per-function table (`-k func`, shown against `perf report` within 0.5 points); cycles sampling and any PMU event (shown by hand) | `func` offered by `cpu.profile`'s catalog entry (one line), inclusive shares, an object key, the frequency-mode samples' period, `perf annotate`; `--pid` attaching in under 500 ms (said now, not faster) |
| `perf mem`, `perf c2c` | the mean latency; the hottest line; the line's offsets, instructions, loads and stores; the delivered sampling rate said | latency by level as a key |
| `perf trace -s`, `perf sched`, `perf lock` | calls, latency, switches, migrations, contentions; per-call records (`proc.calls`), per-switch slices (`sched.slices`), a lock's type (`topk@kind`), the run queue's summary over `wake_to_run_ns` -- shown by hand, no pair yet | `perf trace -s` losing events on every run here, so its pair fails (filed); a lock's caller |
| `strace -c`, `strace -T` | one syscall a run, exact; every syscall in one run (`-k syscall`); per-call records with errno against `strace -e` (shown by hand) | the `errors` column in an aggregating emit; argument decoding (out of scope) |
| `bpftrace`'s tools | the ten above; the `comm` key, per-launch `proc.exec` and `net.tcp.lifetime`, `openat`'s path, a signal's sender, `sched.irq`'s halves (both filter **defects** fixed); tcpaccept, vfsstat, swapin and oomkill's counts as sources (no pair yet) | `runqlen` per cpu; `oomkill`'s victim |
| `vmstat`, `pidstat`, `mpstat`, `sar` | the rows above; `free` (`mem.system`, to the byte), load and run queue (`sched.runqueue`), interrupts (`sched.interrupts`), system `numastat` (`mem.numa --system`), `mpstat`'s per-state split -- served, no pair yet | `pidstat -u`; `mpstat -P ALL` compared; `slabtop` (root's file); `sar`'s history |
| `top`, `htop`, `btop`, `pmap`, `smem` | one process's footprint and cpu; `%Cpu(s)`; the machine's process table (`proc.tasks`, `omn top`'s `system`, against `top -b` by hand); VIRT and SHR | `omn top --batch` as a flag (its output is landed) and its pair; per-mapping rows; a user key |
| `valgrind --tool=dhat`/`massif`, `heaptrack` | totals and the peak; a high-water mark, live blocks, lifetimes | stacks from `mem.heap.alloc` (a **defect**: still refused after the run, its catalog line not landed); attach to a running program |
| `memleak` | outstanding bytes (a gap: `memleak` counts mmap'd memory, `omn` does not) | outstanding by stack; `-p` |
| `ip -s link`, `ss`, `nstat` | bytes, packets (by `dir`), errors, `missed`, `mcast`; rtt and the rest of `tcp_info` (equal to `ss -tin`); every counter (`net.snmp`, against `nstat` by hand); the socket inventory (`net.sockets`, against `ss -tuan` by hand) | pairs for the new sources; `ss -s` |
| `tcpdump` | a filtered count, held with `make CAPTURE=1`; `direction` as a filter field | pcap output; decoding (out of scope) |
| `turbostat`, `cpupower`, `powertop` | energy, busy frequency, idle residency, Mperf; `Busy%` and `Avg_MHz` through APERF/MPERF (turbostat's arithmetic, by hand), the core domain (against `perf stat -a`), an idle cpu's setpoint said, per-process wakeups | pairs for the new attributes; `power.cstate`'s idle share as a counted window; `powertop`'s per-process energy estimate and its machine-wide list |
| `nsys`, `ncu`, `nvidia-smi` | the rows above; per-kernel distributions (`summary@kernel`); the memory clock, throttle reasons (`dmon -s v`'s `pviol`), per-process engine busy (`pmon`'s `sm`), `gpu.mem` under a wrapper -- shown by hand | NVTX; launch facts as values and occupancy; PCIe (measured, waits on a rate unit), ECC (not supported here) |
| `fio`, `iostat -dx` | fio's counts, its mean completion latency (`io.latency` 28 % below, one-sided) and median; `iostat -dx`'s reads and `w/s`; a forked job followed (the **defect** fixed); `%util` (`io.util`, by hand); `dir` on `io.latency` | per-process `io.bytes` and `io.iops`; pairs for `io.util` and `io.vfs` |

### Not replaced

- **No `omn` source**: `slabtop`, `vmstat -s`/`-m`
  ([sysstat](replaces/sysstat.md)); `runqlen` per cpu
  ([bpftrace](replaces/bpftrace.md)); `ss -s`, `tcpdump -w`
  ([net](replaces/net.md)); `powertop`'s per-process estimate
  ([power](replaces/power.md)); `nvidia-smi dmon -s t/e`
  ([gpu](replaces/gpu.md)); `perf sched map`/`replay`, `perf lock record`
  ([perfsched](replaces/perfsched.md)).
- **Interactive only, no batch output to compare**: `htop`, `btop`; `omn
  top`'s batch output is landed in the view library, the flag that prints it
  is not ([top](replaces/top.md)).
- **Out of reach of the mechanism**: dhat's reads and writes per block
  ([heap](replaces/heap.md)); `strace`'s argument decoding.

Each gap is a line in its family's handoffs (`docs/fixes/w22-<family>.md`,
"Gaps", `docs/fixes/w23-<family>.md`, and the twenty-fourth wave's stream
handoffs `docs/fixes/w24-<stream>.md`) and, grouped by the file it lands in,
in `docs/dev/STATE.md`'s "What remains".

## What a verdict rests on

- **Same execution where the tools allow it.** `run: attach`,
  `alongside`, `tool_wraps_omn` and `omn_launches_tool` are one run of the
  workload seen by both; `run: separate` is two runs (medians over `runs`),
  and its `why` says so. On a loaded machine a tolerance across two runs is
  partly noise; the quiet full run at the end of a wave is the verdict.
- **A skip is not a pass.** A pair that skipped here (a tool not installed,
  root not granted, a counter the machine lacks) proves nothing on this
  machine; the family's handoff lists the command that would run it.
- **A disagreement is a finding, not a tolerance.** A pair that misses
  because `omn` is wrong carries `known_defect:` with the finding, and fails
  once the numbers agree; a band is widened only by a mechanism its `why`
  names.
- **A gap is filed, not fixed.** What `omn` lacks for a use is a line of
  work in that family's handoff, not a change in `src/` in a comparison
  wave.
