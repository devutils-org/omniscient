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
