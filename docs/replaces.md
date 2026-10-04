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

Filled in as the families merge, from their tables and the reference runs:
what `omn` replaces **fully** (every common use covered, the same numbers),
what **in part** (which uses, and which numbers agree), and what it **does
not** replace at all, with the line of work each gap became.

### Replaced fully

*(none merged yet)*

### Replaced in part

*(none merged yet)*

### Not replaced

*(none merged yet)*

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
