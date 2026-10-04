# What omn replaces: the `top` family

The pairs are `catalog/references/top.yaml`, the parsers
`tests/reference/top.rs`, and the runner (`tests/reference.rs`) runs them:
`cargo test --test reference pairs_top -- --nocapture`. Every per-process
pair is **one run observed by both** (`run: attach`): the runner starts a
deterministic workload (`tests/reference/workloads/top/hold.py`: 64 MiB
touched, 2 s of its own cpu, its phase in its `comm`) stopped before it
execs, attaches `omn --pid` and the tool, and lets it go; the tool reads at
a known phase through `at.sh`. omn is attached before the program begins,
so TIME+ is one run too. The `%Cpu(s)` pair is `run: alongside`, the same
seconds of the machine. "Measured" below is the runner's, on the
development machine with the twenty-third wave's other streams loading it.

| Tool and use | omn equivalent | Covered | Same numbers | Missing |
|---|---|---|---|---|
| `top -b -p PID`, RES | `omn trace mem.rss --pid PID` (or `-- prog`) | all of it | within 1 % (`top-res`), one run; measured equal | -- |
| `top -b -p PID`, TIME+ | `omn trace cpu.oncpu_time -- prog`, the sum of its series' `count * mean` | all of it for a launched program; for `--pid`, the cpu since the attach | within 30 ms (`top-time`), one run, omn attached before the program's exec; measured 2.0009-2.0015 s over three runs against top's 1.99 s (`bpf:raw_tp`) | the summary of `cpu.oncpu_time` has no `total` field: the pair reads the sum of its series' `count * mean` with a parser of omn's output (`omn_cpu_total`). Not `lifetime.cpu_ns`: that is the launcher's `wait4`, present only for a launched program |
| `top -b -p PID`, %CPU | `omn trace cpu.oncpu_time --pid PID -d 3s --window 1s..3s`, its total over `observed_ns` | the ratio of one cpu over the run (`-e rate` is trace mode only) | within 0.05 (`top-cpu`), one run, the same two seconds (top `-n 2 -d 2` from 1 s after its start, omn's window `1s..3s`); measured 0.002-0.005 apart over three runs | a per-poll ratio in the summary, rather than a total to divide (the pair's parser, `omn_cpu_share`, divides) |
| `top -b -p PID`, VIRT, SHR | none | none | no pair: omn has no source for either | a virtual-size source (`VmSize`) and the resident split (`RssAnon`/`RssFile`/`RssShmem`) as keys of `mem.rss` |
| `top`, the process list of the whole machine sorted by CPU or MEM | `omn top` (system view: gradients of cpu, memory pressure, disk, network) | in part: the machine's totals, not its processes | no pair: no per-process table to compare | a system-wide target for `mem.rss` (it takes pid, cgroup, launch only), and `cpu.oncpu_time --system` without eBPF (only `bpf:raw_tp` serves it); a process table in the system view |
| `top`, the `%Cpu(s)` line | `omn trace cpu.util --system` | the busy ratio (`summary`); the per-state split only outside the summary | within 0.05 (`top-cpus`), the same seconds (`run: alongside`): `top -n 2 -d 2` from 1 s after its start against two 1 s polls over omn's window `1s..3s`; measured 0.0003-0.019 apart over three runs, busy 0.09-0.44. Alternate seconds had differed by up to 0.37 in the twenty-second wave | `cpu.util`'s summary keyed by state (`us`, `sy`, `wa`, `st`) |
| `top`, the `MiB Mem` / `MiB Swap` lines and the load average | the sysstat family's pairs (`free`, `uptime`) | see `docs/replaces/sysstat.md` | no pair: these are the sysstat family's, see `docs/replaces/sysstat.md` | -- |
| `top -H`, threads | `omn trace cpu.oncpu_time --tid TID` (`fs:proc`, one thread per command); every thread of a process at once, `task:<tid>` each, only through `bpf:raw_tp` | per-thread cpu; per-thread memory is the process's (threads share it) | no pair this wave | every thread of a pid in one `fs:proc` run (`/proc/<pid>/task/*/stat`) without eBPF |
| `htop`, the same columns as `top` | as `top` above | as `top` | no pair: htop has no batch mode to parse | the tree view (parent and children of each process) has no omn counterpart |
| `btop`, per-core, memory, disk and network graphs and the process list | `omn top --view system` | in part: the machine's graphs, not its process list | no pair: btop has no batch or text output (`btop --help`: no such option) | the process list, as for `top` |
| `pmap -x PID`, the total RSS | `omn trace mem.rss --pid PID` | all of it | within 1 % (`pmap-x-rss`), one run; measured equal | -- |
| `pmap -x PID`, the per-mapping table | none | none | no pair | a per-mapping key (address range, perms, mapping name) on `mem.rss`/`mem.pss`; the `fs:proc:smaps` backend walks every VMA but sums them |
| `pmap -XX PID`, Rss total | `omn trace mem.rss --pid PID` | all of it | within 1 % (`pmap-xx-rss`), one run; measured equal | -- |
| `pmap -XX PID`, Pss total | `omn trace mem.pss --pid PID` | all of it | within 1 % (`pmap-xx-pss`), one run, omn's last poll (the one nearest pmap's read: PSS moves with every python on the machine); measured 0.01-0.44 % over five runs | -- |
| `pmap -XX PID`, USS (`Private_Clean + Private_Dirty`) | `omn trace mem.uss --pid PID` | all of it | within 1 % (`pmap-xx-uss`), one run; measured equal | -- |
| `pmap -XX PID`, Swap, AnonHugePages, Locked | none | none | no pair | those smaps fields as keys of a memory source |
| `smem -P REGEX`, RSS / PSS / USS of a process | `omn trace mem.rss --pid PID`, `mem.pss`, `mem.uss`, one source per command (or the three in one `-f` session) | all three numbers | RSS and USS within 1 % (`smem-rss`, `smem-uss`), one run, measured equal; PSS (`smem-pss`) omn above smem by at most 2 % and never below, smem being itself a python that maps the interpreter while it reads: measured 0.45-0.89 % over five runs with smem 1.5-8 | one `omn trace` taking several sources, as `smem -c "uss pss rss"` prints them together |
| `smem` (no `-P`), every process of the machine | none | none | no pair | a system-wide target for `mem.pss` and `mem.uss` (they take pid, cgroup, launch; `mem.uss` pid and launch only) |
| `smem -u` / `smem -m`, per user / per mapping | none | none | no pair | a user key; the per-mapping key above |
| `smem -w`, the machine's memory by kind | the sysstat family (`free`) | see `docs/replaces/sysstat.md` | no pair: this is the sysstat family's `free`, see `docs/replaces/sysstat.md` | -- |
