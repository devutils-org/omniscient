# w26 power — idle periods through the tracepoint, and the frequency tracepoint asked whether it fires

Phase 1 of the twenty-sixth wave (`docs/dev/BRIEF.md`, "power").

## Landed

- **`power.cstate` through `bpf:raw_tp`** (`src/bpf/power_idle.rs`,
  `src/collect/power/idle_bpf.rs`): one record per `power:cpu_idle`
  announcement, paired per cpu into idle periods -- one event each, with the
  state, its cpuidle name and when it began. Trace mode alone (the row says
  `modes: [trace]`); counts and polls stay cpuidle's (`fs:sys`). What cannot
  be paired is counted: an exit whose entry was before the window, an entry
  with no exit by the end (both edges, noted), and two entries in a row (a
  lost exit, a loss). It fires without a cpuidle driver too -- the kernel's
  default idle loop announces itself -- so the row requires none.
- **`cpufreq:transitions`**, a requirement on `power.freq`'s `bpf:raw_tp` and
  `bpf:tp` rows: available where a cpufreq driver sets the frequency, missing
  with the reason where the hardware chooses it (`intel_pstate`,
  `amd-pstate-epp` active) or where there is no cpufreq at all (a guest).

## Shown

- `tests/family_power.rs::the_traced_idle_periods_add_up_to_cpuidle_s_own_residency`:
  the traced idle share of a cpu over a 3 s window against cpuidle's own
  `time` counters read around the command, within 0.05. Measured by hand
  first on three cpus: 0.991 against 0.993 (464 periods), 0.997 against
  1.000, 0.974 against 0.975 (1 254 periods).
- Unit tests of the pairing on fabricated announcements, and the program
  built against this kernel.

## Not done, and why

- **`power.freq` through `bpf:raw_tp`**: this machine runs `amd-pstate-epp`,
  so `cpu_frequency` never fires here (`bpftrace`: 0 in 3 s, against 16 424
  `cpu_idle`); a collector could not be shown, and the user's rule is to wait
  for a machine that can. **The distroproof table's +18 cells for it were
  illusory**: in a guest there is no cpufreq and the tracepoint, present,
  never fires. `cpufreq:transitions` now says so.
- **`power.throttle` through `bpf:raw_tp`**: the only thermal zone here is the
  wifi card's (`iwlwifi_1`); the cpu reports its temperature through
  `k10temp` and has no zone with trips to cross. The thermal tracepoints
  would attach and say nothing about the cpu. Waits for a machine whose cpu
  has a thermal zone (an Intel laptop is the common case).
- `bpf:tp` rows: need tracefs readable, root's here.

## Seen

- `scenario_power::every_axis_this_family_declares_is_served_or_refused_by_name`
  failed once in a run beside other work (`power.process_energy` on the test's
  own idle process: no busy tick in 200 ms) and passed alone; not this
  change.
