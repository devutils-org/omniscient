# w26 poller — cumulative counters read at intervals, in one module

Phase 0a of the twenty-sixth wave (`BRIEF.md`, "0a · `poller`").

## Landed

- `src/collect/poll.rs`: `CounterPoll<K, C>` over a `Reading<K, C>` of
  keyed counters that know their own increase (`Cumulative`). It owns the
  four rules every counter poller kept by itself: the baseline at the start,
  a polled run's first tick opening the window (and replacing a baseline
  older than one interval), a counted run's one total at the stop, and a
  counter that appeared or went backwards handed back by key rather than
  folded into a zero. The collector supplies `read` and turns an
  `Interval` into events and losses in its own words.
- Pilots moved onto it: `io/diskstats.rs` (four sources and their
  direction split; the gauge stays the collector's) and `net/snmp.rs`.
  `snmp` gains the stale-baseline rule `diskstats` had alone: a live
  `omn top` attaching for seconds no longer puts the counters' growth
  during the attach into the first interval.

## Shown

- `collect::poll`'s five tests, on fabricated readings: no file read.
- `collect::io::diskstats` (21) and `collect::net::snmp` (5) unchanged and
  green; `tests/family_io.rs` (14), `tests/family_net.rs` (13, with
  `a_known_number_of_refused_connects_moves_the_counters_nstat_names`), and
  `scenario_io`'s two substituted-diskstats tests green.

## Counts

| | before | after |
|---|---|---|
| hand-rolled baseline / first-tick state in the two pilots | 2 + 2 | 0 |
| collectors on the module | 0 | 2 |

The other pollers (`mem/system.rs`, `power/{energy,energy_pmu,cstate}.rs`,
`cpu/util.rs`, `net/procfs.rs`'s users, about twenty in all) move in their
family's phase 1 step.

## Phase 1: the family pollers (2026-10-25)

Moved, one commit each, their tests green: `power.energy` (the wrap is the
counter's own increase), `power.cstate`, `power.throttle`, `power.freq` (its
`/proc/stat` cpu time; the frequency stays a gauge), `mem.oom`, `mem.swap`,
`mem.thp` (its levels carried from `Interval::after`), `sched.interrupts`
(a wrap past 2^32 or a reset is the counter's increase; a line bound
mid-window counts from zero, from `after`).

What the module learned from them:

- `Interval::vanished`: a counter read at the start and not at the end
  (energy's and cstate's "the set changed" check).
- `Cumulative` for a pair, read together (cstate's time and entries).
- `CounterPoll::closing_on_the_last_read`: a counted run over a target that
  can end -- a cgroup removed with its program, a kill landing after the
  last tick -- reads at every tick and closes on the last read that
  succeeded. `mem.oom` and `mem.swap` did this by hand.

What changed for a user: a counted `mem.oom` or `mem.swap` run takes its
baseline at the start, before the program is released, rather than at the
first tick; a polled one keeps a baseline from the start when it is
younger than an interval. `power.throttle` no longer counts a cpu whose
counters appear mid-run from boot.

Left on their own state, with why:

| collector | why it stays |
|---|---|
| `cpu/pressure.rs`, `io/pressure.rs`, `mem/pressure.rs` | their arithmetic is already shared, in `collect::psi`, whose reasons for an unmeasured interval are PSI's own; what each keeps is one `replace` |
| `power/wakeups.rs` (both) | the keys are threads that are born and die: a thread born in the interval counts from zero, a process gone loses its last reading's threads by count -- a process tracker, not a counter poller |
| `cpu/counters.rs` | perf counters, multiplexed, scaled by their own enabled and running times |
| `mem/swapin.rs` | counting kprobes that start at zero when enabled: no baseline to take |

Then `cpu.oncpu_time` and `cpu.throttle` moved too (the user's call,
2026-10-25): their first poll was an interval from the baseline to a tick
taken microseconds after the release -- for `oncpu`, a 10 ms-tick count of
almost nothing, in the series' min and mean. Their first poll now opens the
window, as every other counter poller's does, and their tests take that
opening tick the way the runner does.
