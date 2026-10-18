# `power`: energy, frequency, idle states, throttling

Lazy default: `omn power --system -d 10s` (`power.energy` counted and
`power.freq` polled). The family is the machine's: it runs on `--system` with
a duration, never on a program.

## Which source for which question

| Question | Source | Mode |
|---|---|---|
| How many joules did the machine (package, cores, DRAM) use? | `power.energy` | `count -e summary` (`-k domain` for each RAPL domain) |
| What mean power, in watts? | `power.energy` | `count -e rate` |
| At what frequency did each core actually run? | `power.freq` | `poll@100ms -e timeline` |
| How long in each idle state? | `power.cstate` | `trace` or `poll` |
| How hot? | `power.temp` | `poll@1s` |
| Did it throttle (thermal, power limit)? | `power.throttle` | `poll` |
| How often does my process wake up (powertop)? | `power.wakeups` | `poll@1s`; `--system -k comm -e summary` for every process |
| How much of the package's energy is my process's? | `power.process_energy` | `count -e raw` (an estimate: its share of busy cpu time, said on every event) |
| How idle was each cpu over a command? | `power.cstate` | `count -e rate --system -- cmd` |

## Examples

```console
omn trace power.energy -m count -e summary --system -- ./bench
omn trace power.energy -m count -e summary -k domain --system -d 10s
omn trace power.freq -m poll@100ms -e timeline --system -d 10s
omn trace power.freq -m poll@100ms -e summary --cpu 0-3 -d 10s
omn trace power.cstate,power.temp -m poll@1s -e timeline --system -d 1m
omn trace power.wakeups -m poll@1s -e timeline -- ./daemon
```

A program beside `--system` is the **window**: `power.energy --system --
./bench` is the machine's energy while `./bench` ran, as `perf stat -a`
measures. It is not the program's energy.

## Targets

**RAPL is per package: `--pid` is refused**, never answered with the machine's
number labelled as a process's. A per-process energy is an estimate (its
share of the machine's busy cpu time), and `power.process_energy` is that
estimate, labelled as one on every event. `power.freq`, `power.cstate` take
`--cpu`; `power.wakeups` and `power.process_energy` are the per-process
sources.

## Backends and privilege

- `power.energy` through `pmu` (the perf `power` PMU) is free and needs
  `CAP_PERFMON`; through sysfs (`/sys/class/powercap/.../energy_uj`) it needs
  root on current kernels, because the counter leaks what the machine
  computes. `sudo chmod o+r` on that file after each boot opens it.
- `power.freq` through the `cpu_frequency` tracepoint or APERF/MPERF gives the
  **effective** frequency; sysfs gives what the driver chooses to report.
- On AMD parts and in most virtual machines `thermal_throttle` does not exist,
  and its absence is not evidence that nothing throttled.

## Traps

- **A throttled run is not slower, it is a different machine**: invalidate it
  rather than average it in. The same for a run that crossed a thermal limit.
- The RAPL domains nest (the cores are inside the package): never sum them. The
  counter wraps at about 65 kJ; a decrease is read as one wrap and said.
- One series per cpu, never a mean: one boosted core among idle ones is a mean
  that describes no core.
- Residency is credited when a cpu leaves an idle state, so `idle_ratio` over
  one tick can exceed one.
