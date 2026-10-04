# What omn replaces: the `power` family

Measured on the development machine (AMD, `amd-pstate-epp`, one package, 24
cpus, no `thermal_throttle`), wave 23, through the shared runner
(`cargo test --test reference pairs_power`). The pairs are in
`catalog/references/power.yaml`, the parsers in `tests/reference/power.rs`.
Every `power.*` source is system- or cpu-wide and omn will not launch a
program for one, so each pair runs `alongside`: `omn ... -d 2s` and the tool
wrapping `sleep 2` start together, then the runner launches the load both
watch. The numbers below are three runs of the family, under the wave's
other streams.

| Tool and use | omn equivalent | Covered | Same numbers | Missing |
|---|---|---|---|---|
| `perf stat -a -e power/energy-pkg/ -- prog` | `omn trace power.energy --system -m count -e summary -d 2s` | package energy over a window; not over a program's lifetime | yes, within 5 %: `energy-perf-stat` (238.04 / 237.16 J, 210.13 / 210.03 J, 223.93 / 223.09 J) | a system-wide window bounded by a launched program; `power/energy-core` per core (the `power_core` PMU is here, omn has no core domain) |
| `turbostat --Joules --show Pkg_J -- prog` (and `PkgWatt`) | `omn trace power.energy --system -m count -e summary` (`-e rate` for watts) | package energy and draw | yes, within 5 %: `energy-turbostat` (228.28 / 228.31 J, 200.94 / 200.26 J, 209.52 / 210.01 J) | the program-bounded window, as above |
| `turbostat -c 1 --show Bzy_MHz -- prog` | `omn trace power.freq --cpu 1 -m poll@2s -e summary -d 2s` | busy frequency of a loaded cpu | yes, within 3 %, every cpu loaded: `freq-turbostat` (3924 / 3984, 3974 / 3977, 3938 / 3975 MHz) | an idle cpu: `cpuinfo_avg_freq` reads 1745.6 MHz (the part's lowest non-linear frequency) where turbostat says Bzy 4041, Avg 68 MHz; it is the last scheduler tick's ratio, not the window's, so the pair reads omn's `last`; neither `Avg_MHz` (wall-clock average) nor `Busy%` has an omn counterpart |
| `turbostat`'s `Busy%`, `C1%`, `C2%`, `IPC`, `IRQ` columns | `cpu.util`, `power.cstate`, `cpu.instructions`/`cpu.cycles`, `sched.irq` | in part, through other sources | no pair: those columns are other families' sources | one command printing the per-cpu table turbostat prints; `Busy%` and `Avg_MHz` as `power.freq` attributes |
| `turbostat`'s throttle and `PkgTmp` columns | `power.throttle`, `power.temp` | not on this machine | no pair: AMD has no `thermal_throttle` and turbostat shows no throttle column here | an AMD observable of power-limit throttling (catalog caveat: none exists but the effective frequency) |
| `cpupower monitor -m Idle_Stats -- prog` | `omn trace power.cstate --system -m poll@2s -e summary -d 2s` | per-cpu idle residency over a window | yes, within 0.1 s of 2 s, cpu 1 woken every millisecond so its residency is credited as it accrues: `cstate-cpupower-idle-stats` (cpu 1, eight runs: off by -21 to +64 ms, e.g. 1.256 / 1.253 s, 1.181 / 1.117 s, 1.297 / 1.318 s) | residency per state as one number per state (`state` is a filter, not a key); a `rate` emit (idle share) -- `power.cstate` refuses `-e rate` |
| `cpupower monitor -m Mperf -- prog` | `omn trace power.freq --cpu N -m poll -e summary` | busy frequency | not run: `cpupower` checks for uid 0 itself, so a capability does not do, and `sudo -n` does not grant it here; `freq-cpupower-mperf` skips | `C0`/`Cx` shares as `power.freq` attributes |
| `cpupower monitor -m RAPL` | `omn trace power.energy --system` | package energy | no pair: the monitor prints no column unprivileged here, and the privileged one reads the same counter as `energy-rapl-files` | -- |
| the RAPL files (`energy_uj` before and after) | `omn trace power.energy --system -m count` | package energy | yes: pair `energy-rapl-files`, run unprivileged once `energy_uj` is readable (`chmod o+r`, reset at boot); refused otherwise | -- |
| `sensors` (processor `Tctl` / `Package id 0`) | `omn trace power.temp --system -m snapshot -e summary` | the hottest processor sensor; every sensor as an attribute | yes, within 3 degrees, read side by side: `temp-sensors` (80.875 / 80.875, 79.25 / 79.25, 79.625 / 79.75) | -- |
| `powertop` (overview, per-process power estimate, wakeups, tunables) | `power.energy` for the package draw only | the package draw | no pair: `powertop` is installed but must run as root ("must be run with root privileges"), and its per-process estimate has no omn source to compare | per-process wakeups (timer, irq) and a labelled per-process energy estimate (catalog: a share of cycles); device runtime-PM residency; the tunables are out of scope (omn observes, it does not tune) |
