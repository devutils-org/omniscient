# What omn replaces: the `sysstat` family

The pairs are in `catalog/references/sysstat.yaml`, run by the shared runner
(`cargo test --test reference pairs_sysstat -- --nocapture`); the numbers
below are the runner's, on the development machine (24 cpus, one NUMA node,
loaded by the other streams of the twenty-third wave) on 2026-10-04. "Same
numbers" names the pair by its id. The system pairs (`run: alongside`)
observe the same seconds of the same machine; the launch pairs (`run:
separate`) are two runs of a deterministic workload.

| Tool and use | omn equivalent | Covered | Same numbers | Missing |
|---|---|---|---|---|
| `vmstat 1`: us + sy | `omn cpu.util --system -m poll@1s -e raw` (`busy`) | yes | within 0.03 (abs, as a ratio): seen 0.004, `vmstat-busy` | nothing for the share; its first event covers a few ms (see the handoff) |
| `vmstat 1`: si / so | `omn mem.swap --system -m poll@1s -e raw` (`in_bytes`, `out_bytes`) | yes | within 256 KiB/s, `vmstat-si`, `vmstat-so`; shown only at zero (the machine was not swapping) | a run that swaps, to show a non-zero agreement |
| `vmstat 1`: r, b | none | no | no pair: no source reads `procs_running` / `procs_blocked` | a system run-queue source from `/proc/stat` (`procs_running`, `procs_blocked`) or `/proc/loadavg` |
| `vmstat 1`: swpd, free, buff, cache | none | no | no pair: no system memory source | a `mem.system` source from `/proc/meminfo` |
| `vmstat 1`: bi, bo | `omn io.bytes --system` | in part | no pair here; stream io compares `/proc/diskstats` | vmstat's bi/bo are `pgpgin`/`pgpgout` of `/proc/vmstat` (all block I/O, swap included), not diskstats |
| `vmstat 1`: in, cs | `omn sched.switches --system -m count -e rate`; `sched.irq` counts time, not interrupts | in part | no pair: the system rate carries the same window defect as `pidstat -w` below | an interrupt-count source (`/proc/stat` `intr`); the rate fix |
| `vmstat -s`, `vmstat -m` | none | no | no pair | cumulative counters since boot, and slab caches (as `slabtop`) |
| `free` | none | no | no pair: no system memory source | `mem.system` from `/proc/meminfo`: total, used, free, shared, buff/cache, available, swap |
| `numastat -p PID` | `omn mem.numa -m poll -- prog` (series `node:<n>`, pages) | yes | within 3 %: seen 0.05 %, `numastat-p` | MB as numastat prints them (omn counts pages, THP regions in 2 MiB pages) |
| `numastat` (system: numa_hit, numa_miss, ...) | none | no | no pair: `mem.numa` is per process | a system source from `/sys/devices/system/node/node*/numastat` |
| `slabtop` | none (`mem.kmalloc` counts allocation flow, not the slab stock) | no | no pair: `/proc/slabinfo` is root-only, and no source reads it | a `mem.slab` source from `/proc/slabinfo` (privileged) |
| `uptime`: load average | none | no | no pair: no source reads `/proc/loadavg` | a `sched.load` source from `/proc/loadavg` |
| `cat /proc/pressure/cpu` | `omn cpu.pressure --system -m poll@1s -e raw` (`some`) | in part | within 0.02 (abs): seen below 0.0001 at a 6 % share, `psi-cpu-some` | `full` as a share of the window (only `full_avg10` and `full_total_us` are given); its attribute names differ from `mem.pressure`/`io.pressure` |
| `cat /proc/pressure/memory` | `omn mem.pressure --system -m poll@1s -e raw` (`stall_ratio`) | yes | within 0.02, `psi-memory-some`; shown only at zero (no memory pressure) | nothing |
| `cat /proc/pressure/io` (some, full) | `omn io.pressure --system -m poll@1s -e raw` (`stall_ratio`, `full_stall_ratio`) | yes | within 0.02: seen 0.003 (`some`, 8.6 %) and 0.0014 (`full`, 6.2 %), `psi-io-some`, `psi-io-full` | nothing |
| `mpstat 1` (all cpus) | `omn cpu.util --system -m poll@1s -e raw` | yes | within 0.03: seen 0.0016, `mpstat-all` | nothing for `all` |
| `mpstat -P ALL` | `omn cpu.util --cpu 0-23 -m poll@1s` | in part | no pair yet for the per-cpu rows | a per-cpu pair (keyed `cpu`) |
| `mpstat -I` | `sched.irq` (time per handler, not counts) | no | no pair | interrupt counts per cpu (`/proc/interrupts`, `/proc/softirqs`) |
| `iostat -c` | `omn cpu.util --system -m poll@1s -e raw` | yes | within 0.03: seen 0.007, `iostat-c` | nothing |
| `iostat -d`, `iostat -x` | `io.iops`, `io.bytes`, `io.latency`, ... | see `docs/replaces/io.md` | no pair: the device columns are the io family's pairs | |
| `pidstat -r` | `omn mem.rss -m poll -- prog` | yes | within 3 %: seen 0.45 %, `pidstat-r` | minflt/s, majflt/s per process as a rate |
| `pidstat -w` | `omn sched.switches -m count -e rate -- prog` | in part | **no**: omn 77 135/s against 931/s, `pidstat-w` (known defect) | the rate's window (handoff); voluntary vs involuntary needs `force:bpf:raw_tp` |
| `pidstat -u` | `omn cpu.oncpu_time -m poll -- prog` | in part | no pair: omn gives cumulative ns, pidstat a %CPU per interval, and `poll` cannot produce `rate` | a `rate` emit on poll, and the tail after the last poll (the last second of a run is lost) |
| `pidstat -d` | `omn io.bytes --pid` (bpf only) | in part | no pair yet | a pair under the capabilities |
| `sar -u` (live) | `omn cpu.util --system -m poll@1s -e raw` | yes | within 0.03: seen 0.012, `sar-u` | nothing |
| `sar` (history: `sa1`/`sadc` files, `-f`) | `omn` sessions and `omn diff` | in part | no pair | a collector that runs unattended and keeps days of history |
