# Wave 22, stream power -- handoff

Branch `feat/w22-power`. Files: `catalog/references/power.yaml` (7 pairs),
`tests/reference/power.rs` (5 parsers, 9 tests), `docs/replaces/power.md` (11
rows), `tests/reference/workloads/power/` (`spin.py`, `rapl_window.sh`),
`tests/reference/captures/power/`. Nothing outside the stream's files.

## What landed

| Pair | Tool | Runs here | Tolerance | Measured by hand, same window |
|---|---|---|---|---|
| `power.energy` | `perf stat -a -e power/energy-pkg/` | yes | rel 0.05 | 143.14 / 145.92 J idle-ish, 173.59 / 178.08 J loaded (1.9 %, 2.6 %) |
| `power.energy` | `turbostat --no-msr --Joules` | yes, unprivileged | rel 0.05 | 85.61 / 88.37 J, 251.81 / 258.16 J (3.1 %, 2.5 %) |
| `power.energy` | the RAPL files (`rapl_window.sh`) | no: `energy_uj` is root-only | rel 0.05 | not run |
| `power.freq` | `turbostat -c 1 --show Bzy_MHz` | yes, unprivileged | rel 0.03 | 3994.47 / 3994 MHz, every cpu loaded |
| `power.freq` | `cpupower monitor -m Mperf` | no: reads `/dev/cpu/*/msr` | rel 0.03 | not run |
| `power.cstate` | `cpupower monitor -m Idle_Stats` | yes | abs 0.1 s per cpu | cpu 0 1.685 / 1.704 s, cpu 1 1.765 / 1.774 s, cpu 12 1.290 / 1.312 s |
| `power.temp` | `sensors -j` | yes | abs 3 degrees | 50.625 / 50.625 (Tctl) |

`turbostat` is in the brief's "needs root" list, but `--no-msr` runs it
unprivileged here: it reads RAPL, APERF/MPERF and the C-states through perf
(`perf_event_paranoid` is -1 on this machine). The pairs use `--no-msr`.

## Shown vs reasoned

- **Shown**: every number in the table above, run by hand with the pair's
  exact tool command wrapping omn's exact arguments (and the background
  workload for the loaded ones); the 9 parser tests pass on captures. The
  captures of `perf stat`, `turbostat`, `cpupower monitor -m Idle_Stats` and
  `sensors -j` are real outputs from this machine.
- **Reasoned, not shown**: the pairs have not run through
  `tests/reference.rs`, because the runner (stream reference) is not on `main`
  yet and needs the fields below. `cpupower_mperf.txt` and the two
  `rapl_window*.txt` captures are written by hand in the tools' format (both
  need root); the Mperf column layout follows `cpupower monitor`'s table, not
  an observed run.

## Not done, and why

- **No pair for `power.throttle`**: this AMD part has no `thermal_throttle`,
  turbostat shows no throttle column, and omn reports the source out of
  reach. Nothing on this machine could witness it.
- **No pair for `powertop`**: not installed; its overview's draw is the same
  RAPL counter the energy pairs cover, and its per-process estimate has no omn
  source to compare against.
- **No `cpupower monitor -m RAPL` pair**: it prints an empty column
  unprivileged, and privileged it reads what the RAPL-files pair reads.
- **turbostat's other columns** (`Busy%`, `Avg_MHz`, `IPC`, `IRQ`) are rows
  without pairs: they map onto other families' sources.

## Gaps in omn (lines of work for later waves)

1. `omn trace <system-scoped source> -- prog`: measure a system or cpu
   window bounded by a launched program, the way `perf stat -a -- prog` and
   `turbostat prog` do. Today `--system` and `-- PROGRAM` are exclusive and
   the per-launch form is refused, so every power pair has the tool wrap omn.
2. `power.freq` through `fs:sys` on an idle cpu reports 1745.645 MHz on every
   idle core -- the part's `amd_pstate_lowest_nonlinear_freq` -- where
   turbostat says Bzy_MHz 4041 and Avg_MHz 68 for the same window. The value
   is neither the busy frequency nor the wall-clock average; the event should
   say so, or the backend should carry busy share alongside.
3. `power.freq` attributes for `Busy%` and `Avg_MHz` (turbostat's two other
   frequency views).
4. `power.cstate -e rate` (idle share of the window) is refused; `cpupower
   monitor` and `turbostat` both print shares.
5. `power.energy` has no core domain: the `power_core` PMU
   (`power_core/energy-core/`) is here and `perf stat` reads it.
6. Per-process wakeups and a labelled per-process energy estimate
   (`powertop`'s main view).

## Privileged runs wanted

```console
# the two privileged pairs (energy_uj and cpupower's Mperf)
sudo -v && cargo test --test reference -- power
# or grant without sudo, scoped:
sudo setcap cap_sys_rawio+ep /usr/bin/cpupower   # Mperf reads /dev/cpu/*/msr (also needs the msr module, which this wave does not load)
sudo chmod o+r /sys/class/powercap/intel-rapl:0/energy_uj   # resets at boot
# powertop, for its rows
sudo pacman -S powertop
```

## Decision lines

- Power pairs run the tool around omn (`same_run: tool_wraps_omn`), not omn
  around a workload, because omn refuses to launch a program for a
  system-scoped source; the tool's window is omn's plus ~45 ms of startup,
  and the tolerances carry it.
- Parsers convert units on the tool's side (`*_MHz` also as `*_Hz`, cpupower
  percentages into nanoseconds over the command's duration) so the runner
  compares like for like without a scale field.
- Parser names carry the family prefix (`power_*`) so they cannot collide in
  `all_parsers()`.

## Requests to other streams

- **reference** (runner): honour `same_run: tool_wraps_omn` (run
  `reference.command ++ [target/debug/omn] ++ omn`, nothing appended; the
  tool's stdout and stderr both hold omn's output too, and omn's JSON is the
  `{`..`}` block on stdout), `same_run: back_to_back` (omn, then the tool
  alone), `background: true` (start `workload` first, kill it after), and
  `compare.omn_series` (the omn series `compare.omn` reads; default: the
  series named like `compare.key`). Until it does, these pairs should skip
  with "same_run not supported" rather than run omn with a workload, which
  fails.
- The toolchain changed mid-wave on this machine (rustc lost
  `libLLVM.so.22.1` once, then rebuilt on LLVM 23); a stream that sees that
  error should simply rebuild.
