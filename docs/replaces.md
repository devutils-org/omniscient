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

From the eleven tables at the twenty-second wave's merge. **Read this
first**: the numbers below were shown by each family's own harness (an
ignored test, a `run_pairs.py`, a `mod runner`) or by hand on the pair's
exact commands. The shared runner did not exist when the families wrote
their pairs, and at the merge it compares none of them: `pairs_bpftrace`
passes because its pairs skip on `sudo -n`, `pairs_net` because its pairs
declare a `gap:` pointing at the family's harness, and the other nine
families fail it (`docs/open-findings.md`, "The twenty-second wave,
coordinator, after the merge"). A verdict here becomes the runner's when its
family's pairs run through `cargo test --test reference`.

### Replaced fully

**No tool as a whole.** These common uses are covered and their numbers
agree:

- `perf stat -e <event> -- prog` for instructions, cycles, IPC,
  branch-misses, cache-misses, page-faults, context-switches and
  cpu-migrations, per process, system-wide (`-a`) and per cgroup (`-G`);
  `perf stat -p PID` per on-cpu nanosecond ([perfstat](replaces/perfstat.md)).
- `perf record` + `perf report --sort dso,sym` self shares, `perf top -p`,
  `perf mem`'s mean load latency, `perf c2c`'s hottest line
  ([perfrecord](replaces/perfrecord.md)).
- `strace -c -w` for one syscall's count and wall time, `strace -T`'s mean
  on a long call ([perfsched](replaces/perfsched.md)).
- `vmstat`'s busy share, `/proc/pressure/*`, `mpstat`, `iostat -c`, `sar -u`
  live, `numastat -p`, `pidstat -r` ([sysstat](replaces/sysstat.md)).
- `top -b -p`'s RES, TIME+ and `%CPU`, its `%Cpu(s)` line; `pmap -x` and
  `-XX` totals (RSS, PSS, USS); `smem -P`'s RSS, PSS and USS
  ([top](replaces/top.md)).
- dhat's total bytes and blocks and its `t-end`; heaptrack's calls and leaked
  bytes ([heap](replaces/heap.md)).
- `ip -s link`'s bytes; `nstat`'s listen overflows, listen drops and
  `TcpRetransSegs`; `ss -ti`'s rtt ([net](replaces/net.md)).
- Package energy against `perf stat -a -e power/energy-pkg/` and
  `turbostat`; `turbostat`'s busy frequency; `cpupower monitor -m
  Idle_Stats`; `sensors` ([power](replaces/power.md)).
- nsys's launches, copies and driver-call counts per kernel and call, and
  kernel device time; ncu's instruction, block, L1 and DRAM counters;
  nvidia-smi's power, shader clock, utilisation, temperature and
  per-process VRAM, query and `dmon` ([gpu](replaces/gpu.md)).
- fio's completed reads, bytes and operations ([io](replaces/io.md)).

### Replaced in part

| Tool | What `omn` serves, and agrees on | What it lacks (the gap, filed) |
|---|---|---|
| `perf stat` | the rows above | `branches`; several events on one launch; `-r N`; task-clock as a source; `-a -- prog` bounded by a program |
| `perf record`/`report`, `perf top` | self shares by function and object | a per-function table in `trace`'s output, inclusive shares compared, cycles sampling, any PMU event, `perf annotate` |
| `perf mem`, `perf c2c` | the mean latency; the hottest line | latency by level as a key; the line's offsets, code addresses and pids |
| `perf trace -s`, `perf sched`, `perf lock` | calls, latency, switches, migrations, contentions (**pairs not run**: tracefs is root's here) | `syscall` as a key; errors; per-call and per-switch records; a summary of `wake_to_run_ns`; lock kind and caller as keys |
| `strace -c`, `strace -T` | one syscall a run, exact | every syscall in one run; errors; argument decoding (out of scope) |
| `bpftrace`'s tools | syscount, opensnoop, killsnoop, execsnoop, bitesize, biosnoop, biolatency, runqlat, tcpconnect, tcplife: `omn`'s side exact against the workload (**tool side not run**: root) | a `comm` key; per-launch `proc.exec` and `net.tcp.lifetime`; opened paths, senders; two filter **defects** (`comm` on renamed tasks, `sched.irq`'s `kind`) |
| `vmstat`, `pidstat`, `mpstat`, `sar` | the rows above | memory, load and run queue, interrupts; `pidstat -w`'s rate (a **defect**: the window); `pidstat -u`; `mpstat -P ALL` compared; `sar`'s history |
| `top`, `pmap`, `smem` | one process's footprint and cpu | the machine's process table; VIRT and SHR; per-mapping rows; a user key |
| `valgrind --tool=dhat`/`massif`, `heaptrack` | totals and the peak (shown only where the workload holds it) | stacks from `mem.heap.alloc` (a **defect**: refused after the run); a high-water mark; live blocks; lifetimes; attach to a running program |
| `memleak` | outstanding bytes (**pair not run**: root) | outstanding by stack; `-p` |
| `ip -s link`, `ss -ti` | bytes, packets, errors at zero; rtt | `net.packets` by direction; `missed`, `mcast`; `tcp_info`'s other fields |
| `tcpdump` | a filtered count (**pair not run**: root and `CAP_NET_RAW`) | pcap output; decoding (out of scope) |
| `turbostat`, `cpupower` | energy, busy frequency, idle residency (Mperf and the RAPL files **not run**: root) | `Busy%` and `Avg_MHz`; an idle share; the core domain; an idle cpu's reading said for what it is |
| `nsys`, `ncu`, `nvidia-smi` | the rows above | per-kernel distributions; NVTX; launch facts and occupancy; PCIe, throttle reasons, ECC; per-process engine busy; `gpu.mem` under a wrapper |
| `fio`, `iostat -dx` | fio's counts; its mean completion latency within 25 % (the layers above the device); `iostat -dx` **not run** when the stream ran | `io.latency` following a forked job (a **defect**); `%util`; per-process `io.bytes` and `io.iops` |

### Not replaced

- **No `omn` source**: `free`, `uptime`'s load, `slabtop`, system
  `numastat`, `vmstat -s`/`-m` ([sysstat](replaces/sysstat.md));
  `runqlen`, `tcpaccept`, `oomkill`, `vfsstat`, `swapin` per process
  ([bpftrace](replaces/bpftrace.md)); `nstat`'s other counters, `ss -tan`,
  `ss -tlnp`, `ss -s`, `tcpdump -w` ([net](replaces/net.md)); `powertop`'s
  per-process estimate ([power](replaces/power.md)); `nvidia-smi dmon -s
  v/t/e` ([gpu](replaces/gpu.md)); `perf sched map`/`replay`, `perf lock
  record` ([perfsched](replaces/perfsched.md)).
- **Interactive only, no batch output to compare**: `htop`, `btop`, and
  `omn top` itself ([top](replaces/top.md)).
- **Out of reach of the mechanism**: dhat's reads and writes per block
  ([heap](replaces/heap.md)); `strace`'s argument decoding.

Each gap is a line in its family's handoff (`docs/fixes/w22-<family>.md`,
"Gaps") and, grouped by the file it lands in, in `STATE.md`'s "What
remains".

## What a verdict rests on

- **Same execution where the tools allow it.** A pair with `run: attach`
  is one run of the workload seen by both; `run: separate` is two runs, and
  its `why` says so. On a loaded machine a tolerance across two runs is
  partly noise.
- **A skip is not a pass.** A pair that skipped here (a tool not installed,
  root not granted, a counter the machine lacks) proves nothing on this
  machine; the family's handoff lists the command that would run it.
- **A gap is filed, not fixed.** What `omn` lacks for a use is a line of
  work in that family's handoff (`docs/fixes/w22-<family>.md`), not a change
  in `src/` this wave.
