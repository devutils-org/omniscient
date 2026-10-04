# What omn replaces: the `power` family

Measured on the development machine (AMD, `amd-pstate-epp`, one package, 24
cpus, no `thermal_throttle`), wave 22. The pairs are in
`catalog/references/power.yaml`, the parsers in `tests/reference/power.rs`.
Every `power.*` source is system- or cpu-wide and omn will not launch a
program for one, so each pair has the tool launch `omn ... -d 2s` and both
read the same window (`same_run: tool_wraps_omn`).

| Tool and use | omn equivalent | Covered | Same numbers | Missing |
|---|---|---|---|---|
| `perf stat -a -e power/energy-pkg/ -- prog` | `omn trace power.energy --system -m count -e summary -d 2s` | package energy over a window; not over a program's lifetime | within 5 % (1.9 % and 2.6 % measured; 2.3 % is the tool's longer window): pair `power.energy` / `perf stat` | a system-wide window bounded by a launched program; `power/energy-core` per core (the `power_core` PMU is here, omn has no core domain) |
| `turbostat --Joules --show Pkg_J -- prog` (and `PkgWatt`) | `omn trace power.energy --system -m count -e summary` (`-e rate` for watts) | package energy and draw | within 5 % (3.1 % and 2.5 % measured): pair `power.energy` / `turbostat` | the program-bounded window, as above |
| `turbostat -c 1 --show Bzy_MHz -- prog` | `omn trace power.freq --cpu 1 -m poll@2s -e summary -d 2s` | busy frequency of a loaded cpu | within 3 % (3994.47 against 3994 MHz, every cpu loaded): pair `power.freq` / `turbostat` | an idle cpu: `cpuinfo_avg_freq` reads 1745.6 MHz (the part's lowest non-linear frequency) where turbostat says Bzy 4041, Avg 68 MHz; neither `Avg_MHz` (wall-clock average) nor `Busy%` has an omn counterpart |
| `turbostat`'s `Busy%`, `C1%`, `C2%`, `IPC`, `IRQ` columns | `cpu.util`, `power.cstate`, `cpu.instructions`/`cpu.cycles`, `sched.irq` | in part, through other sources | no pair this wave | one command printing the per-cpu table turbostat prints; `Busy%` and `Avg_MHz` as `power.freq` attributes |
| `turbostat`'s throttle and `PkgTmp` columns | `power.throttle`, `power.temp` | not on this machine | no pair: AMD has no `thermal_throttle` and turbostat shows no throttle column here | an AMD observable of power-limit throttling (catalog caveat: none exists but the effective frequency) |
| `cpupower monitor -m Idle_Stats -- prog` | `omn trace power.cstate --system -m poll@2s -e summary -d 2s` | per-cpu idle residency over a window | within 0.1 s of 2 s per cpu (cpu 0 1.685 / 1.704 s, cpu 1 1.765 / 1.774 s, cpu 12 1.290 / 1.312 s): pair `power.cstate` / `cpupower monitor` | residency per state as one number per state (`state` is a filter, not a key); a `rate` emit (idle share) -- `power.cstate` refuses `-e rate` |
| `cpupower monitor -m Mperf -- prog` | `omn trace power.freq --cpu N -m poll -e summary` | busy frequency | not run: Mperf reads `/dev/cpu/*/msr`, root only (pair `power.freq` / `cpupower monitor`, privileged) | `C0`/`Cx` shares as `power.freq` attributes |
| `cpupower monitor -m RAPL` | `omn trace power.energy --system` | package energy | no pair: the monitor prints no column unprivileged here, and the privileged one reads the same counter as the RAPL-files pair | -- |
| the RAPL files (`energy_uj` before and after) | `omn trace power.energy --system -m count` | package energy | not run: `energy_uj` is root-only (pair `power.energy` / `the RAPL files`, privileged) | -- |
| `sensors` (processor `Tctl` / `Package id 0`) | `omn trace power.temp --system -m snapshot -e summary` | the hottest processor sensor; every sensor as an attribute | within 3 degrees, read back to back (50.625 against 50.625): pair `power.temp` / `sensors` | -- |
| `powertop` (overview, per-process power estimate, wakeups, tunables) | `power.energy` for the package draw only | the package draw | no pair: `powertop` is not installed, and its per-process estimate has no omn source to compare | per-process wakeups (timer, irq) and a labelled per-process energy estimate (catalog: a share of cycles); device runtime-PM residency; the tunables are out of scope (omn observes, it does not tune) |
