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

From the eleven tables at the twenty-third wave's merge. **Every verdict
here is the shared runner's**: each family's pairs run through `cargo test
--test reference`, and the tally below is the coordinator's run on a quiet
machine at `1edb677` (2026-10-04; `docs/open-findings.md`, "The twenty-third
wave, coordinator, after the merge").

| Family | Pairs | Held | Skipped | Gaps | Known defects | Failed |
|---|---|---|---|---|---|---|
| [perfstat](replaces/perfstat.md) | 13 | 13 | 0 | 0 | 0 | 0 |
| [perfrecord](replaces/perfrecord.md) | 5 | 5 | 0 | 0 | 0 | 0 |
| [perfsched](replaces/perfsched.md) | 9 | 9 | 0 | 0 | 0 | 0 |
| [bpftrace](replaces/bpftrace.md) | 10 | 9 | 0 | 1 | 0 | 0 |
| [sysstat](replaces/sysstat.md) | 13 | 12 | 0 | 0 | 1 | 0 |
| [top](replaces/top.md) | 11 | 10 | 0 | 0 | 0 | 1 |
| [heap](replaces/heap.md) | 9 | 8 | 0 | 1 | 0 | 0 |
| [net](replaces/net.md) | 8 | 8 | 0 | 0 | 0 | 0 |
| [power](replaces/power.md) | 7 | 6 | 1 | 0 | 0 | 0 |
| [gpu](replaces/gpu.md) | 21 | 21 | 0 | 0 | 0 | 0 |
| [io](replaces/io.md) | 7 | 7 | 0 | 0 | 0 | 0 |
| **all** | **113** | **108** | **1** | **2** | **1** | **1** |

The privileged families (perfsched, bpftrace, heap, net, power) are from the
coordinator's run after the merge, with `/etc/sudoers.d` granting the
witnesses alone without a password and each wrapper sending only its tool
through `sudo -n` (`reference.sudo`); the others from the full run at
`1edb677`. The one skip is `cpupower monitor -m Mperf`: `cpupower` checks for
uid 0 itself and `sudo -n` does not grant it. The two gaps are scope, not
numbers: `runqlat.bt` watches the machine over a program's run, which `omn`
cannot bound yet, and `memleak` counts mmap'd memory, which
`mem.heap.live` does not. The known defect is `pidstat -w` against
`sched.switches`' counted rate; the failure is `top-cpus`, which held when
rerun alone, at the edge of its band.

### Replaced fully

**No tool as a whole.** These common uses are covered, and a pair holds
through the runner:

- `perf stat -e <event> -- prog` for instructions, cycles, IPC,
  branch-misses, cache-misses, page-faults, context-switches,
  cpu-migrations and task-clock, per process, system-wide (`-a`) and per
  cgroup (`-G`); `perf stat -p PID`'s raw count
  ([perfstat](replaces/perfstat.md)).
- `perf record` + `perf report --sort dso,sym` self shares, `perf top -p`,
  `perf mem`'s mean load latency (on a quiet machine: the two time different
  mixes of loads, filed), `perf c2c`'s hottest line
  ([perfrecord](replaces/perfrecord.md)).
- `strace -c -w` for one syscall's count and wall time, `strace -T`'s mean
  on a long call ([perfsched](replaces/perfsched.md)).
- `vmstat`'s busy share (and `si`/`so`, at zero), `/proc/pressure/*`,
  `mpstat`, `iostat -c`, `sar -u` live, `numastat -p`, `pidstat -r`
  ([sysstat](replaces/sysstat.md)).
- `top -b -p`'s RES, TIME+ and `%CPU`; `pmap -x` and `-XX` totals (RSS,
  PSS, USS); `smem -P`'s RSS, PSS and USS ([top](replaces/top.md)).
- dhat's total bytes and blocks, `t-gmax` and `t-end`; massif's peak;
  heaptrack's calls, peak and leaked bytes ([heap](replaces/heap.md)).
- `ip -s link`'s bytes and packets; `nstat`'s listen overflows, listen
  drops and `TcpRetransSegs`; `ss -ti`'s rtt ([net](replaces/net.md)).
- Package energy against `perf stat -a -e power/energy-pkg/` and
  `turbostat`; `turbostat`'s busy frequency; `cpupower monitor -m
  Idle_Stats`; `sensors` ([power](replaces/power.md)).
- nsys's launches, copies and driver-call counts per kernel and call, kernel
  and copy device time; ncu's instruction, block, L1 and DRAM counters;
  nvidia-smi's power, shader clock, utilisation, temperature and
  per-process VRAM, query and `dmon` ([gpu](replaces/gpu.md)).
- fio's completed reads, bytes and operations; `iostat -dx`'s `r/s`,
  `rkB/s` and `r_await` ([io](replaces/io.md)).

### Replaced in part

| Tool | What `omn` serves, and agrees on | What it lacks (the gap, filed) |
|---|---|---|
| `perf stat` | the rows above | `branches`; several events on one launch; `-r N`; task-clock as a source; `-a -- prog` bounded by a program; `--pid` ending with its process |
| `perf record`/`report`, `perf top` | self shares by function and object | a per-function table in `trace`'s output, inclusive shares compared, cycles sampling, any PMU event, `perf annotate`; `--pid` attaching in under 500 ms |
| `perf mem`, `perf c2c` | the mean latency; the hottest line | latency by level as a key; the IBS sampler's delivered rate said, its `peer` level checked; the line's offsets, code addresses and pids |
| `perf trace -s`, `perf sched`, `perf lock` | calls, latency, switches, migrations, contentions (held with root; `perf trace -s` loses calls, so its count is a lower bound) | `syscall` as a key; errors; per-call and per-switch records; a summary of `wake_to_run_ns`; lock kind and caller as keys |
| `strace -c`, `strace -T` | one syscall a run, exact | every syscall in one run; errors; argument decoding (out of scope) |
| `bpftrace`'s tools | syscount, opensnoop, killsnoop, execsnoop, bitesize, biosnoop, biolatency, runqlat, tcpconnect, tcplife: nine held with root, exact or within a few events; runqlat a gap of scope | a `comm` key; per-launch `proc.exec` and `net.tcp.lifetime`; opened paths, senders; two filter **defects** (`comm` on renamed tasks, `sched.irq`'s `kind`) |
| `vmstat`, `pidstat`, `mpstat`, `sar` | the rows above | memory, load and run queue, interrupts; `pidstat -w`'s rate (a **defect**, the pair's `known_defect`: the window); `pidstat -u`; `mpstat -P ALL` compared; `sar`'s history |
| `top`, `pmap`, `smem` | one process's footprint and cpu; `%Cpu(s)` within 0.05 (**failed once** in the quiet run, `omn` below; filed) | the machine's process table; VIRT and SHR; per-mapping rows; a user key; `cpu.util` by state |
| `valgrind --tool=dhat`/`massif`, `heaptrack` | totals and the peak (shown where the workload holds it) | stacks from `mem.heap.alloc` (a **defect**: refused after the run); a high-water mark; live blocks; lifetimes; attach to a running program |
| `memleak` | outstanding bytes (a gap: `memleak` counts mmap'd memory, `omn` does not) | outstanding by stack; `-p` |
| `ip -s link`, `ss -ti` | bytes, packets, errors at zero; rtt | `net.packets` by direction; `missed`, `mcast`; `tcp_info`'s other fields |
| `tcpdump` | a filtered count, equal by hand once `lo`'s second copy of each frame is left out (held with root and `make CAPTURE=1`) | `direction` as a filter field; pcap output; decoding (out of scope) |
| `turbostat`, `cpupower` | energy, busy frequency, idle residency (the RAPL files held once readable; Mperf skips: `cpupower` wants uid 0) | `Busy%` and `Avg_MHz`; an idle share; the core domain; an idle cpu's reading said for what it is; `power.freq`'s first poll before the load |
| `nsys`, `ncu`, `nvidia-smi` | the rows above | per-kernel distributions; NVTX; launch facts and occupancy; PCIe, throttle reasons, ECC; per-process engine busy; `gpu.mem` under a wrapper |
| `fio`, `iostat -dx` | fio's counts; its mean completion latency with `io.latency` 28 % below (the layers above the device; a one-sided 50 %); `iostat -dx`'s reads | `io.latency` following a forked job (a **defect**); `%util`; per-process `io.bytes` and `io.iops`; a write pair; a `dir` key on `io.latency` |

### Not replaced

- **No `omn` source**: `free`, `uptime`'s load, `slabtop`, system
  `numastat`, `vmstat -s`/`-m` ([sysstat](replaces/sysstat.md));
  `runqlen`, `tcpaccept`, `oomkill`, `vfsstat`, `swapin` per process
  ([bpftrace](replaces/bpftrace.md)); `nstat`'s other counters, `ss -tan`,
  `ss -tlnp`, `ss -s`, `tcpdump -w` ([net](replaces/net.md)); `powertop`'s
  per-process estimate ([power](replaces/power.md); installed, root-only);
  `nvidia-smi dmon -s v/t/e` ([gpu](replaces/gpu.md)); `perf sched
  map`/`replay`, `perf lock record` ([perfsched](replaces/perfsched.md)).
- **Interactive only, no batch output to compare**: `htop`, `btop`, and
  `omn top` itself ([top](replaces/top.md)).
- **Out of reach of the mechanism**: dhat's reads and writes per block
  ([heap](replaces/heap.md)); `strace`'s argument decoding.

Each gap is a line in its family's handoffs (`docs/fixes/w22-<family>.md`,
"Gaps", and `docs/fixes/w23-<family>.md`) and, grouped by the file it lands
in, in `STATE.md`'s "What remains".

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
