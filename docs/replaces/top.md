# What omn replaces: the `top` family

The pairs are `catalog/references/top.yaml`, the parsers
`tests/reference/top.rs`. Every pair runs a deterministic workload
(`tests/reference/workloads/top/hold.py`: 64 MiB touched, 2 s of its own cpu)
through `onpid.py`, which launches it and hands the tool its pid. Ten of the
eleven pairs are **one run observed by both** (`same_run: onpid_attach`):
onpid.py also starts `omn --pid` on the same workload. The TIME+ pair alone
is alternate runs, because TIME+ counts from the process's start and an
attach counts from the attach. Until `tests/reference.rs` runs pairs,
`tests/reference/workloads/top/run_pairs.py` runs them all. "Measured" below
is three rounds of it on the development machine.

| Tool and use | omn equivalent | Covered | Same numbers | Missing |
|---|---|---|---|---|
| `top -b -p PID`, RES | `omn trace mem.rss --pid PID` (or `-- prog`) | all of it | within 1 % (`mem.rss` / `top` / `res`), one run; measured 0.00 % (equal) | -- |
| `top -b -p PID`, TIME+ | `omn trace cpu.oncpu_time -- prog`, the sum of its series' `count * mean` | all of it for a launched program; for `--pid`, the cpu since the attach | within 30 ms (`cpu.oncpu_time` / `top` / `time`), alternate runs; measured 1.99 s against 2.0027-2.0043 s (`bpf:raw_tp`), 1.99 s through `fs:proc` | the summary of `cpu.oncpu_time` has no `total` field: the pair reads `count * mean` (`compare.omn_jq`). Not `lifetime.cpu_ns`: that is the launcher's `wait4`, present for every launched source, so it says nothing of this one |
| `top -b -p PID`, %CPU | `omn trace cpu.oncpu_time --pid PID -d 2s`, its total over `observed_ns` | the ratio of one cpu over the run (`-e rate` is trace mode only) | within 0.05 (`cpu.oncpu_time` / `top` / `cpu`), one run against top's second iteration (`-n 2 -d 1`); measured 0.003 apart | a per-poll ratio in the summary, rather than a total to divide |
| `top -b -p PID`, VIRT, SHR | none | none | no pair: omn has no source for either | a virtual-size source (`VmSize`) and the resident split (`RssAnon`/`RssFile`/`RssShmem`) as keys of `mem.rss` |
| `top`, the process list of the whole machine sorted by CPU or MEM | `omn top` (system view: gradients of cpu, memory pressure, disk, network) | in part: the machine's totals, not its processes | no pair: no per-process table to compare | a system-wide target for `mem.rss` (it takes pid, cgroup, launch only), and `cpu.oncpu_time --system` without eBPF (only `bpf:raw_tp` serves it); a process table in the system view |
| `top`, the `%Cpu(s)` line | `omn trace cpu.util --system` | the busy ratio (`summary`); the per-state split only outside the summary | within 0.05 (`cpu.util` / `top` / `busy`), the same seconds: both started together by onpid.py, `top -n 2 -d 2` against two 1 s polls; measured 0.001-0.009 apart over four runs, busy from 0.01 to 0.54. Alternate seconds had differed by up to 0.37 | `cpu.util`'s summary keyed by state (`us`, `sy`, `wa`, `st`) |
| `top`, the `MiB Mem` / `MiB Swap` lines and the load average | the sysstat family's pairs (`free`, `uptime`) | see `docs/replaces/sysstat.md` | see there | -- |
| `top -H`, threads | `omn trace cpu.oncpu_time --tid TID` (`fs:proc`, one thread per command); every thread of a process at once, `task:<tid>` each, only through `bpf:raw_tp` | per-thread cpu; per-thread memory is the process's (threads share it) | no pair this wave | every thread of a pid in one `fs:proc` run (`/proc/<pid>/task/*/stat`) without eBPF |
| `htop`, the same columns as `top` | as `top` above | as `top` | no pair: htop is not installed here, and has no batch mode to parse | the tree view (parent and children of each process) has no omn counterpart |
| `btop`, per-core, memory, disk and network graphs and the process list | `omn top --view system` | in part: the machine's graphs, not its process list | no pair: btop has no batch or text output (`btop --help`: no such option) | the process list, as for `top` |
| `pmap -x PID`, the total RSS | `omn trace mem.rss --pid PID` | all of it | within 1 % (`mem.rss` / `pmap` / `""`), one run; measured 0.00 % | -- |
| `pmap -x PID`, the per-mapping table | none | none | no pair | a per-mapping key (address range, perms, mapping name) on `mem.rss`/`mem.pss`; the `fs:proc:smaps` backend walks every VMA but sums them |
| `pmap -XX PID`, Rss total | `omn trace mem.rss --pid PID` | all of it | within 1 % (`mem.rss` / `pmap` / `rss`), one run; measured 0.00 % | -- |
| `pmap -XX PID`, Pss total | `omn trace mem.pss --pid PID` | all of it | within 1 % (`mem.pss` / `pmap` / `pss`), one run; measured 0.01-0.04 %. Alternate runs had been 1.0-1.5 % apart, onpid.py's python3 sharing the interpreter on one side only | -- |
| `pmap -XX PID`, USS (`Private_Clean + Private_Dirty`) | `omn trace mem.uss --pid PID` | all of it | within 1 % (`mem.uss` / `pmap` / `uss`), one run; measured 0.00 % | -- |
| `pmap -XX PID`, Swap, AnonHugePages, Locked | none | none | no pair | those smaps fields as keys of a memory source |
| `smem -P REGEX`, RSS / PSS / USS of a process | `omn trace mem.rss --pid PID`, `mem.pss`, `mem.uss`, one source per command (or the three in one `-f` session) | all three numbers | within 1 % each (`mem.rss`/`mem.pss`/`mem.uss` / `smem`), one run; measured with smem 1.5 (Debian's script, run in place; no Arch package): RSS and USS equal, PSS 0.27-0.29 %, smem being itself a python3 that maps the interpreter while it reads | one `omn trace` taking several sources, as `smem -c "uss pss rss"` prints them together |
| `smem` (no `-P`), every process of the machine | none | none | no pair | a system-wide target for `mem.pss` and `mem.uss` (they take pid, cgroup, launch; `mem.uss` pid and launch only) |
| `smem -u` / `smem -m`, per user / per mapping | none | none | no pair | a user key; the per-mapping key above |
| `smem -w`, the machine's memory by kind | the sysstat family (`free`) | see `docs/replaces/sysstat.md` | see there | -- |
