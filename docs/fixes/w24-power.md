# The twenty-fourth wave, stream power

Branch `feat/w24-power`. Defect first, then the four features the brief
names. Commits: `d3f3d6e` (the defect, and busy/idle on `fs:sys`), `e88e1c3`
(`power.freq` through `pmu`), `2f721c3` (the core energy domain and the
`domain` key), `5a3a76b` (`power.wakeups`), `7d4f169` (the scenarios
document), and this handoff.

**Said first: one file outside the owned set.** `src/event.rs` gains
`KeyValue::Domain { name }`, its constructor, its `kind()` arm and its
`Display` arm. It is the other half of the `KeyKind` append seam in
`src/request.rs` (a key nobody can construct is no key); appended only, no
existing arm touched. `src/request.rs` gains `KeyKind::Domain` (variant, doc
line, `ALL` 14 -> 15, `as_str` arm) and one unit test. A merge with another
stream's key keeps both variants and sums the `ALL` length.

## What landed

1. **Defect: `power.freq`'s first poll at window open.** The read at window
   open is now the baseline, through both backends, and produces no event;
   each event covers the interval since the previous read (`interval_ns`).
   `-m poll@2s -d 2s` is one reading per cpu, at the close.
2. **Busy share, average, idle share** on every `power.freq` event: `busy`,
   `idle`, `avg_hz` beside `hz`.
   - New backend **`pmu`** (`src/collect/power/freq_pmu.rs`): APERF, MPERF
     and the TSC as perf events of the kernel's `msr` unit, which this user
     opens without root (paranoid -1 here; `CAP_PERFMON` otherwise).
     turbostat's arithmetic exactly: `busy = dMPERF/dTSC` (Busy%), `hz =
     tsc_hz * dAPERF/dMPERF` (Bzy_MHz), `avg_hz = dAPERF/dt` (Avg_MHz).
     Its requirements are now `pmu:msr/aperf/`, `pmu:msr/mperf/`,
     `pmu:msr/tsc/` (they were `msr:aperf`, `msr:mperf`, which probe
     `/dev/cpu/*/msr`, root's). It comes **before `fs:sys`** in the chain,
     so it is what `power.freq` resolves to here.
   - `fs:sys`: `busy`/`idle` from `/proc/stat` per cpu over the interval,
     `avg_hz = busy * hz`.
3. **`power.freq` on an idle cpu says what it reads.** On this kernel
   `cpuinfo_avg_freq` is the last scheduler tick's APERF/MPERF, and on a cpu
   with no tick in the last 20 ms it falls back to the policy's setpoint
   (1745645 kHz here, `scaling_min_freq`). A reading exactly equal to one of
   the setpoints the driver declares is now `measures=setpoint`, and `basis`
   names the file and says the cpu was idle. The `Average` basis no longer
   claims to be "averaged over the interval".
4. **The core energy domain.** `power.energy --keys domain` gives one series
   per domain; AMD's per-core `power_core/energy-core` is opened on each cpu
   of the unit's cpumask and summed into `domain:cores` (`counters=12`).
   **Unkeyed, the package alone is read** -- the domains nest, and the one
   unkeyed series used to sum package + cores + dram wherever the part
   published them (Intel), counting the cores twice. `fs:sys` does not carry
   the key (`keys: []`), and `-k domain --backend force:fs:sys` is refused
   naming `pmu`.
5. **Per-process wakeups**: new source `power.wakeups`, `fs:proc`, `--pid` or
   a launched program, poll. The increase of every thread's
   `voluntary_ctxt_switches` per interval (each voluntary switch is one sleep,
   ended by one wakeup); `per_second`, `threads`, `threads_exited`. A thread
   that exits between two reads is counted in `dropped`, with a reason.

## Shown vs reasoned

- **Shown, against turbostat** (`sudo -n turbostat --quiet --cpu 1,2,3 --show
  CPU,Avg_MHz,Busy%,Bzy_MHz sleep 2` started with `omn trace power.freq --cpu
  1,2,3 -m poll@2s -d 2s -e raw`, a spin loop pinned to cpu 2 and a 50 %-duty
  loop on cpu 1, under the other streams' load, load average 14):
  - `pmu`: cpu 2 Busy 99.996 / 100.00 %, Bzy 4084.0 / 4084 MHz; cpu 1 Busy
    65.6 / 63.3 %, Avg 2678 / 2584 MHz; cpu 3 72.4 / 67.9 % (an unloaded cpu
    under others' load, the two windows offset by omn's start-up).
  - `fs:sys`: cpu 1 Busy 50.2 / 50.9 %, Avg 2119.5 / 2121 MHz; cpu 2 100 /
    100 %; cpu 3 79.1 / 79.5 %. `hz` 4218 / Bzy 4261 MHz (the last tick's
    ratio, not the interval's).
- **Shown, the idle fallback**: before the fix, `power.freq --cpu 0,1`
  through `fs:sys` read cpu 0 at exactly 1745645000 Hz on every poll, with
  `basis=...averaged over the interval`; `scaling_min_freq` is 1745645.
- **Shown, against `perf stat -a -x, -e power/energy-pkg/,
  power_core/energy-core/ sleep 2`** started with `omn trace power.energy
  --system -m count -d 2s -e raw` (then keyed): cores 226.89 / 227.77 J,
  package 276.77 / 277.31 J.
- **Shown, against a workload of known shape**: `omn trace power.wakeups -m
  poll@200ms -e raw -- python3 -c 'for i in range(500): time.sleep(0.002)'`:
  5 events, 94 + 97 x 4 = 482 wakeups of 500 sleeps, 485/s (one sleep plus
  the loop is 2.06 ms: 1.04 s / 500 = 2.08 ms); the missing 18 are the last
  40 ms between the last poll and the exit.
- **Reasoned, not shown**: the Intel `energy-cores` path keeps the package
  domain's road (opened per package leader) and only differs by being keyed;
  no Intel machine here. The setpoint detection on drivers other than
  `amd-pstate-epp` (the file list includes `base_frequency`, Intel's).
- The `fs:sys` road into `power.energy` happened to be readable this session
  (`energy_uj` is `-r-----r--` after someone's chmod), so
  `the_two_roads_to_rapl_agree_where_both_are_open` ran instead of skipping,
  and held.

## Tests run

- `cargo test --lib collect::power` -- 54 passed (new: the `/proc/stat`
  parser, baseline and shares, idle setpoint, three `freq_pmu`, four
  `wakeups`, one against this machine each).
- `cargo test --test scenario_power --test family_power` -- 21 + 15 passed
  (new scenarios: `a_core_held_busy_reads_busy_through_every_road_and_once_per_interval`,
  `the_core_domain_is_every_cores_counter_summed_and_less_than_its_package`,
  `a_program_that_sleeps_three_hundred_times_wakes_three_hundred_times`;
  `every_axis_...` now asks a source without a system target about the test's
  own pid; two family tests updated for the `pmu` road).
- Outside the owned files, because the change reaches them: `cargo test --lib
  request:: event:: catalog` (45, 7, 61 passed), `cargo test --test
  join_engine` on the three `power.freq` joins (3 passed), `cargo test --test
  cross_machine` (6 passed), `cargo test --test reference families_are_whole`
  (passed), `cargo test --test cli` (33 passed).
- `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt
  --check`: clean, after `git merge main` (triage, unwind merged).

## Not done, and why

- **`power.cstate -e rate`** (an idle share of the window from cpuidle):
  `rate` is a `count`-mode emit, and the cpuidle collector is poll-only; a
  counted cstate window is a new mode for the collector, not started. The
  idle share itself is on every `power.freq` event now (`idle`, through both
  roads), which is what turbostat and `cpupower monitor` print.
- **`power.wakeups --system`** (powertop's whole-machine list): needs a walk
  of every process keyed by `proc`, which is the machine-wide process source
  stream proc lands this wave; reusing it is the next step. Today the source
  refuses `--system` and names `sched.wakeup`.
- **The per-process energy estimate** (powertop's other column): not started;
  the catalog's caveat stands (a share of cycles, labelled as an estimate).
- ~~`power.wakeups` misses the last partial interval of a launched
  program~~: recovered once `main` was merged (see "Review", 1).

## Review

The fix round, after `git merge main` (lazy, run, triage, unwind and the
others in).

1. **`power.wakeups`: uncounted loss at the exit** (`wakeups.rs`). Two halves.
   - *The launched program's last interval.* Once `main` was merged, it is no
     longer lost: `8193889` (`fix(launch)`, not in this branch when the
     handoff above was measured) sees the exit with `WNOWAIT` and keeps the
     program a zombie until the collectors' closing round, and that round
     reads the zombie leader's `voluntary_ctxt_switches`. The reviewer's
     command, `omn power.wakeups -m poll@100ms -e summary -- python3 -c '300
     sleeps of 2 ms'`, now gives 7 events, mean 43, 301 wakeups (raw: 43 49
     48 49 48 49 15; the 15 is the partial interval). So its "no loss" is
     true there now. The scenario's lower bound goes from 150 to 285, so a
     lost last interval (about 15 % of that run) fails it.
   - *Wherever a read still finds the process gone* (a `--pid` reaped by its
     own parent, a launched program whose zombie is already reaped): `sample`
     now counts every thread of the previous read as `dropped`, noting "the
     process was gone at a read, and its wakeups since the previous one are
     not counted", before it returns `TargetGone`. Gone before any read
     measured nothing and stays clean. Test:
     `a_process_gone_after_a_read_counts_its_last_interval_as_lost`.
   - *A thread born and gone between two reads* is in neither listing. procfs
     keeps no running total of a process's dead threads' switches (the
     signal struct's `nvcsw` reaches userspace only through the reap's
     rusage), so it cannot be counted. The catalog caveat and
     `docs/scenarios/power.md` now say so.
2. **`power.freq` through `pmu`: a dropped interval** (`freq_pmu.rs`).
   `Interval::between` returns `Result<Interval, Undivided>`. A counter read
   lower than before, or no time passing (clock or TSC), is
   `Undivided::Backwards` / `NoTime`: `failed_reads += 1` with the cpu
   named. A cpu whose MPERF did not move is a measurement now: an event with
   `busy=0`, `idle=1`, `avg_hz`, `measures=idle` and a basis that says so,
   and **no `hz`**, since it ran at no clock. Summary, hist and stack count
   such an event as unmeasured (`emit::measured`). Timeline and rate read
   the value through `emit::value_of`, which refuses an event without it.
   That refusal is wrong for this case, but emit is a shared layer and not
   this stream's to change (see "Needs from another stream"). Over 120
   events of `power.freq --system -m poll@100ms -d 500ms -e timeline` on
   this machine no cpu had a zero-MPERF interval, so the timeline was not
   refused here. Test:
   `an_idle_cpu_is_a_measurement_and_a_backwards_counter_is_a_fault`.
3. **`power.energy --keys domain`: the `cores` series vanishing**
   (`energy_pmu.rs`). `per_core_domain` returns `Result<Option<Opened>,
   String>`. `Ok(None)` means only that the part has no `power_core` unit.
   Each of these is an `Err` naming the file, cpu or counter: an unreadable
   or unparsable cpumask, a core whose counter will not open or enable, no
   scale, no cpu of the package in the mask, and an unreadable
   `physical_package_id`. The last one no longer means package 0. The `Err`
   reaches the output through `Collector::notes` ("package N's `cores`
   domain is not read, though the part counts it per core: ..."). The rest
   of the run still measures. Not shown failing on this machine, where every
   core opens: `-k domain` still prints `domain:cores` and `domain:package`.

Tests, fix round: `cargo test --lib collect::power` 55 passed; `cargo test
--test scenario_power --test family_power` 21 + 15 passed; `cargo test --lib
catalog` 63 passed (the caveat changed); `cargo clippy --workspace
--all-targets -- -D warnings` and `cargo fmt --check` clean.

## Privileged runs wanted

None new. turbostat ran through `sudo -n` as granted. The reference pairs
should be rerun by stream reference or the coordinator, because the backend
under them moved:

```console
cd /home/moha/repos/omniscient && make && cargo test --test reference pairs_power -- --nocapture
```

## Decision lines

- `power.freq` prefers APERF/MPERF through the `msr` perf unit over sysfs:
  it opens unprivileged, every number covers the interval, and an idle cpu
  reads the clock it ran at when it woke rather than a setpoint.
- A polled source whose reading describes the moment before it takes its
  window-open read as a baseline, not an event.
- Energy domains are a key (`domain`); unkeyed, `power.energy` reads the
  package alone, since domains nest and one series across them double-counts.
- A wakeup per process is a voluntary context switch: each is one sleep,
  ended by one wakeup.

## Needs from another stream

- **reference**: `catalog/references/power.yaml` -- `freq-turbostat` and
  `freq-cpupower-mperf` now resolve to `pmu` and get one reading at the
  close; `.last` still holds and `.mean` would now mean the interval's
  (its `why` saying the first reading precedes the load is stale). Energy
  pairs read `series[key=system]`, which unkeyed is still the package: no
  change needed. New pairs worth adding: `Busy%`/`Avg_MHz` against turbostat
  (`busy`, `avg_hz` attributes), `power_core/energy-core/` against `perf stat
  -a` (`--keys domain`, `series[key=domain:cores]`), `pidstat -w` cswch/s
  against `power.wakeups`.
- **emit**: `timeline_by` and `rate_by` read the value through `value_of`,
  which refuses the whole output (as "`hz` is not a numeric attribute") when
  one event lacks it. Summary, hist and stack count such an event as
  unmeasured. `power.freq` through `pmu` now leaves `hz` off a cpu that
  never left idle over an interval (Review, 2). Timeline should skip and
  count it the same way.
- **emit**: the summary is over `hz` only; `busy`, `idle` and `avg_hz` reach
  the reader through `raw` and the JSON events. A summary over a named
  attribute would let `-e summary` print turbostat's three columns.
- **views / top**: `view/src/views/system.rs` asks `power.energy` with
  `keys: &[]` and fans out on the `domain` attribute; unkeyed it now gets the
  package alone (it got package alone on this AMD machine before, too). To
  show the cores, ask with `keys: &["domain"]`.
- **Coordinator**: `docs/CATALOG.md`'s power section (the `pmu` road of
  `power.freq` and its requirements, the `domain` key, `power.wakeups`),
  `man/omn.1`, `docs/dev/STATE.md` (power 2-6 of the twenty-second wave's gaps: 2, 3,
  5 and 6 done for a process; 4 through `power.freq`'s `idle`).
