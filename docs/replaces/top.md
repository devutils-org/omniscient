# What omn replaces: the `top` family

The pairs are `catalog/references/top.yaml`, the parsers
`tests/reference/top.rs`. Every pair runs a deterministic workload
(`tests/reference/workloads/top/hold.py`: 64 MiB touched, 2 s of its own cpu)
once under omn and once under the tool, through `onpid.py` where the tool
wants a pid -- alternate runs, held to a tolerance its `why` explains.
"Measured" below is three runs each on the development machine under the
twenty-second wave's load.

| Tool and use | omn equivalent | Covered | Same numbers | Missing |
|---|---|---|---|---|
| `top -b -p PID`, RES | `omn trace mem.rss --pid PID` (or `-- prog`) | all of it | within 1 % (`mem.rss` / `top` / `res`); measured 0.05 % | -- |
| `top -b -p PID`, TIME+ | `omn trace cpu.oncpu_time -- prog`, the results' `lifetime.cpu_ns` | all of it for a launched program; a pid's total only as a sum of poll deltas | within 30 ms (`cpu.oncpu_time` / `top` / `time`); measured 1.99 s against 2.002-2.007 s | the summary of `cpu.oncpu_time` has no `total`: a pid's cpu over the run is `count * mean`, not a field, so the pair reads the reap's `cpu_ns` |
| `top -b -p PID`, %CPU | `omn trace cpu.oncpu_time --pid PID -e rate` | the rate, per poll | no pair this wave | a pair against top's second iteration (`-n 2 -d 1`) |
| `top -b -p PID`, VIRT, SHR | none | none | no pair: omn has no source for either | a virtual-size source (`VmSize`) and the resident split (`RssAnon`/`RssFile`/`RssShmem`) as keys of `mem.rss` |
| `top`, the process list of the whole machine sorted by CPU or MEM | `omn top` (system view: gradients of cpu, memory pressure, disk, network) | in part: the machine's totals, not its processes | no pair: no per-process table to compare | a system-wide target for `mem.rss` (it takes pid, cgroup, launch only), and `cpu.oncpu_time --system` without eBPF (only `bpf:raw_tp` serves it); a process table in the system view |
| `top`, the `%Cpu(s)` line | `omn trace cpu.util --system` | the busy ratio (`summary`); the per-state split only outside the summary | no pair: alternate seconds of a shared machine differ by up to 0.37 (busy 0.25 against 0.62); parser `top_batch_cpus` is ready | the runner running omn and the tool over the same seconds; `cpu.util`'s summary keyed by state (`us`, `sy`, `wa`, `st`) |
| `top`, the `MiB Mem` / `MiB Swap` lines and the load average | the sysstat family's pairs (`free`, `uptime`) | see `docs/replaces/sysstat.md` | see there | -- |
| `top -H`, threads | `omn trace cpu.oncpu_time --tid TID` (`fs:proc`, one thread per command); every thread of a process at once, `task:<tid>` each, only through `bpf:raw_tp` | per-thread cpu; per-thread memory is the process's (threads share it) | no pair this wave | every thread of a pid in one `fs:proc` run (`/proc/<pid>/task/*/stat`) without eBPF |
| `htop`, the same columns as `top` | as `top` above | as `top` | no pair: htop is not installed here, and has no batch mode to parse | the tree view (parent and children of each process) has no omn counterpart |
| `btop`, per-core, memory, disk and network graphs and the process list | `omn top --view system` | in part: the machine's graphs, not its process list | no pair: btop has no batch or text output (`btop --help`: no such option) | the process list, as for `top` |
| `pmap -x PID`, the total RSS | `omn trace mem.rss --pid PID` | all of it | within 1 % (`mem.rss` / `pmap` / `""`); measured 0.17 % | -- |
| `pmap -x PID`, the per-mapping table | none | none | no pair | a per-mapping key (address range, perms, mapping name) on `mem.rss`/`mem.pss`; the `fs:proc:smaps` backend walks every VMA but sums them |
| `pmap -XX PID`, Rss total | `omn trace mem.rss --pid PID` | all of it | within 1 % (`mem.rss` / `pmap` / `rss`) | -- |
| `pmap -XX PID`, Pss total | `omn trace mem.pss --pid PID` | all of it | within 3 % (`mem.pss` / `pmap` / `pss`); measured 1.0-1.5 %, the launcher's own python3 sharing the interpreter | -- |
| `pmap -XX PID`, USS (`Private_Clean + Private_Dirty`) | `omn trace mem.uss --pid PID` | all of it | within 1 % (`mem.uss` / `pmap` / `uss`); measured 0.02 % | -- |
| `pmap -XX PID`, Swap, AnonHugePages, Locked | none | none | no pair | those smaps fields as keys of a memory source |
| `smem -P REGEX`, RSS / PSS / USS of a process | `omn trace mem.rss --pid PID`, `mem.pss`, `mem.uss`, one source per command (or the three in one `-f` session) | all three numbers | pairs `mem.rss`/`mem.pss`/`mem.uss` against `smem` skip here: smem is not installed | one `omn trace` taking several sources, as `smem -c "uss pss rss"` prints them together |
| `smem` (no `-P`), every process of the machine | none | none | no pair | a system-wide target for `mem.pss` and `mem.uss` (they take pid, cgroup, launch; `mem.uss` pid and launch only) |
| `smem -u` / `smem -m`, per user / per mapping | none | none | no pair | a user key; the per-mapping key above |
| `smem -w`, the machine's memory by kind | the sysstat family (`free`) | see `docs/replaces/sysstat.md` | see there | -- |
