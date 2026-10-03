# The eighteenth wave, stream bench

Branch `feat/w18-bench`, from `main` at `9cd5409`. One item: `STATE.md`
"After the seventeenth wave" item 9, the bench's bands corrected to the table
that ran before the wave (`target/bench/backends-1791030430/` in the main
checkout, read there, not copied).

## What landed

- **`catalog/*.yaml`, 43 `cost:` lines corrected** to the band the table
  measured: 27 `free` to `low` (every perf counter in `count`, the
  system-wide `/proc` and `/sys` polls, `proc.exec` and `proc.exit`,
  `proc.fd`, `proc.threads`, `runtime.jit`, `gpu.temp`, `power.freq`,
  `power.temp`); to `med`: `cpu.profile` through `pmu:sw`, `gpu.engine_busy`,
  `gpu.mem`, `io.latency` and `io.queue` through `bpf:raw_tp`, `mem.numa`,
  `mem.pss`, `mem.rss` and `mem.uss` through `smaps_rollup` (and `smaps` for
  the two that have it), `net.tcp.rtt` through `netlink`; to `high`:
  `gpu.freq`, `gpu.power`, `io.size` through `bpf:raw_tp`, `power.cstate`.
  No other line of any entry changed. No band was lowered.
- **Two stand, with their reason**: `io.fsync` (`high`) and `net.tcp.connect`
  (`low`) through `bpf:fentry`. Their 142 % and 39 % are the same ~55 ms
  fentry attach on two workloads of different length, on runs that saw no
  event: a fixed cost, not a rate. Their dilation (+6.3 %, zero) is inside the
  declared band, and the test holds them to it.
- **The 46th pair, `mem.swap` through `fs:cgroup`**, was over `free` only on a
  +0.21 % dilation of a workload the gate says resolves 0.39 %. The rule the
  record is judged by reads a dilation no larger than the workload's
  resolution as zero; under it the row is within `free`.
- **`docs/bench.md`, "The bands, corrected to the second table, eighteenth
  wave"**: the rule, what each correction says (the tool's floor is above
  `free`; a poll is priced by what it reads; the raw-tracepoint ring has a
  price per event; `cpu.profile` at 4.9 %, eight times the tenth wave's), what
  stands and why, what was not corrected, and the record itself between
  `<!-- bands:begin backends-1791030430 -->` and `<!-- bands:end -->`: the
  gate's resolution per workload and all 95 rows of the table (share,
  dilation, the band declared when the table ran, the band declared now, the
  verdict).
- **`tests/bench_bands.rs`**, five tests in `make check` (eight after the
  fix round, below), none measuring the
  machine: the catalog declares exactly the band each row records (so a cost
  line cannot change without its row); every row's measurement is inside its
  band or the row says why it stands; a standing row's reason is still needed
  and its dilation is inside the band; `corrected` iff the band changed; and
  the rule on synthetic rows.

## Review

The fix round answers the adversarial review of `6672a46`.

- **Defect: an equivalent `bpf:tp` was a band under its corrected
  `bpf:raw_tp`** (`io.size`, `io.queue`, `io.latency`, `proc.exec`,
  `proc.exit`), so `--budget` promised to degrade onto the same tracepoint as
  if it were cheaper. Fixed both ways the reviewer offered: the siblings are
  raised (`bpf:tp` to `high`, `med`, `med`, `low`, `low`), and
  `tests/bench_bands.rs::no_equivalent_backend_declares_less_than_what_it_stands_in_for`
  refuses an `equivalent` backend declaring less than a backend above it that
  serves a target it serves. The rule found two more: `power.freq` through
  `pmu` (aperf/mperf, `free` under `fs:sys`'s `low`) and `net.tcp.rtt` through
  `bpf:fentry` (`low` under `netlink`'s `med`), both raised -- the second is
  unmeasured, and a budget must not promise a saving nobody measured.
  `io.pressure`'s `fs:cgroup` (`free` under `fs:proc`'s `low`) is exempt:
  the two serve disjoint targets. The reviewer's repro,
  `omn io.size -m trace -e hist --system --budget 1% --dump-request`, now
  says nothing below `bpf:raw_tp` declares less, as `main` did. The rule
  lives in the test, not the catalog loader (a shared layer); filed below.
- **Gap: the judging logic was not shown failing.** Factored into
  `record_faults` and `band_faults`, each run on the real record by one test
  and on synthetic rows by another, one row per way of being wrong: over its
  band on the share and on the dilation, `corrected` and still over, a
  `stands` reason empty (three spellings) or no longer needed, a standing
  row's dilation outside its band, `corrected` with no move, a move with no
  `corrected`, a verdict that is none of the three, a workload with no
  resolution, a row twice, a row missing.
- **Nit: the record's completeness.** The marker is now `<!-- bands:begin
  backends-1791030430 rows=95 -->` and the test fails when the record holds
  another count, or when the marker gives none; verdicts are parsed to
  `within`, `corrected` or `stands ...: <reason>`, anything else fails.
  Shown: deleting `io.size`'s row fails `the_record_is_whole_and_reads_one_way`;
  putting `io.size`'s `bpf:tp` back to `low` fails the equivalence test.
- **Nit: the resolution floor and `CATALOG.md`** are engine's files; the
  requests below stand, with the seven new lines added to the `CATALOG.md`
  one.
- **Nit: count-mode `pmu` left `free`.** `mem.cache_llc` through `pmu` is
  now `low`, beside its measured `pmu:generic` (the same counters).
  `power.freq` through `pmu` moved under the equivalence rule. What stays
  `free`, and why, is in `docs/bench.md`: `sched.switches` through `pmu:sw`
  measured 0.06 % (so the floor is the hardware counters', and the doc now
  says so), `power.energy` through RAPL 0.03 %, and `mem.bandwidth` through
  `pmu:uncore` has no collector to measure.

## Shown versus reasoned

- **Shown**: the 46 over-band pairs, read from `table.json`/`table.md`; that
  after the change every one of the 95 rows is inside its band under the rule
  or stands with a reason (`bench_bands`, green); that reverting one corrected
  line (`cpu.cycles` through `pmu` back to `free`) fails
  `every_measured_pair_declares_the_band_its_row_records` naming it; that
  `budget`, `recorder`, `runner`, `scenario_power`, `catalog_model`, `cli` and
  the catalog unit tests stay green with the new bands.
- **Shown from the table**: the counters' cost is a rate, not a fixed opening
  cost (about 240 us on a 145 ms target, 1.1 to 1.2 ms on a 355 ms one); the
  fentry attach is fixed (about 55 ms on `io` and on `compute`, and 64 and
  68 ms in the tenth wave's table).
- **Reasoned, not measured**: that the two fentry rows' cost on a long target
  is their dilation (no table ran a long target through them); that
  `io.latency`'s `med` holds at `null_blk`'s load (the bench's `io` workload is
  far from it); that the seven unmeasured lines raised in the fix round cost
  what their measured neighbour does (same tracepoint, same counters), and,
  for `net.tcp.rtt` through `bpf:fentry`, no less than `netlink` -- a ceiling
  chosen so a budget does not promise a saving, not a measurement.

## What I could not do, and why

- **Apply the resolution floor in the harness's own `band` column**:
  `harness::judge_band` is in `examples/bench/harness.rs`, stream engine's
  file. Filed below. Until it lands, the next table will flag `mem.swap`
  through `fs:cgroup` again where `bench_bands` does not.
- **Re-measure**: `scripts/bench.sh` is not a stream's to run. Wanted below.
- **Correct `CATALOG.md`'s cost column**: stream engine's file. Filed below.

## Privileged runs wanted

Alone on the machine, after the wave's merge (so `cpu.profile` and the io
histogram carry their streams' fixes), from the main checkout:

```console
cd /home/moha/repos/omniscient && make && scripts/bench.sh backends
```

Then paste the new table into the record in `docs/bench.md` (the medians of
`table.md`'s `% of target` and `dilation` columns, the gate's `RESOLVES`
column, the band declared, the verdict) and run
`cargo test --test bench_bands`: it names every band the new table puts
outside, and every `stands` whose reason is gone. `cpu.profile` is the row
most likely to move back down, `io.latency` the most likely to move up.

## Decision lines for DECISIONS.md

- **The cost bands follow the bench's table, and a test holds them to it**
  (wave 18): 43 bands corrected to the second table; `tests/bench_bands.rs`
  reads the record in `docs/bench.md` and fails when a cost line and its row
  disagree, or a row is over its band with no reason. Reason: the bands are
  what `--budget` promises in advance, and they had drifted from the
  measurement by up to two bands with nothing failing.
- **A row's cost is the larger of the tool's share and the target's
  dilation, a dilation within the workload's resolution reading as zero**
  (wave 18). Reason: an in-kernel collector's cost is in the target
  (finding 47), and a dilation the gate cannot resolve is not a measurement.
- **A fixed per-run cost has no band** (wave 18): `io.fsync` and
  `net.tcp.connect` through `bpf:fentry` keep their bands on their dilation;
  their ~55 ms attach is a fixed cost, still counted by `cost.json`. Reason:
  a band is a percentage of the target's cpu, and a fixed cost is one only
  once a length is chosen.
- **`free` means under 0.1 %, and the perf counters are not under it**
  (wave 18): every `count`-mode counter measured 0.13 to 0.20 % and is `low`.
  Reason: the band's ceiling is a number (`Cost::worst_case_percent`), and
  the measurement is over it on every workload the counters ran on.

- **An equivalent backend declares no cheaper band than what it stands in
  for** (wave 18, fix round): held by `tests/bench_bands.rs`, for backends
  serving a common target. Reason: `--budget` degrades onto what is below and
  declares less, and an equivalent backend is the same measurement -- often
  the same tracepoint -- so a cheaper band on it promises a saving the move
  cannot deliver.

## Needs from another stream

- **engine (`examples/bench/harness.rs`)**: `judge_band` takes the
  workload's resolution and reads a dilation no larger than it as zero, as
  `tests/bench_bands.rs::measured` does, so the table's `band` column and the
  record agree. The gate's resolutions are already in the table's header.
- **engine or coordinator (`src/catalog.rs`, the loader)**: if the rule
  "an `equivalent` backend declares no cheaper band than a backend above it
  serving a target it serves" belongs with the other chain invariants,
  move `cheaper_equivalents` from `tests/bench_bands.rs` into the loader's
  validation. The catalog satisfies it today.
- **engine (`CATALOG.md`)**: the cost column for the rows whose bands moved,
  including the fix round's (`io.size`, `io.queue`, `io.latency` through
  `bpf:tp`; `proc.exec`, `proc.exit` through `bpf:tp`; `power.freq` through
  `pmu`; `net.tcp.rtt` through `bpf:fentry`, `med`; `mem.cache_llc` through
  `pmu`, `low`):
  `cpu.cycles`, `cpu.instructions`, `cpu.ipc`, `cpu.branch_misses`,
  `cpu.cache_misses`, `cpu.context_switches`, `cpu.migrations` (`free` to
  `low`); `cpu.profile` (`low-med` stays right); `io.size` (`low` to `high`
  for `bpf:raw_tp`); `net.tcp.rtt` (`free-low` to `med` for `netlink`);
  `power.cstate` (`low` to `high` for `fs:sys`); `gpu.freq` / `.power` /
  `.temp` (`free` to `high`, `high`, `low` through `nvml`); `proc.exec` /
  `.exit` (`free` to `low`); and the `mem.*` smaps rows (`med`).
- **launcher (`src/collect/mod.rs` or wherever `cost.json` weighs the
  collector's cpu)**: report the attach (`bpf:fentry`'s ~55 ms trampoline
  rebuild) apart from the run's rate, or leave it out of what `--budget`
  weighs, so a short `io.fsync` or `net.tcp.connect` run under a budget its
  band fits is not stopped by a fixed cost. Not urgent; filed so the band's
  reason is not read as hiding it.
- **profile, io**: your sources' bands are now `med` (`cpu.profile` through
  `pmu:sw`, `io.latency` through `bpf:raw_tp`), what the table measured
  before your work. A re-measured table lowers them; nothing else should.
- **Coordinator**: the bench run above, and the four decision lines.

## The suite

On `b753af0`, with ten streams sharing the machine (load average 27 to 37):

- **`make check`**: red on its first test failure,
  `cross_io::the_crossing_sessions_run_and_say_what_they_found` (2000 reads,
  3995 matched: another stream's disk traffic inside its window), entry 25
  of `docs/flaky.md`. Rerun alone once: passes.
- **`cargo test --workspace --no-fail-fast`**: 2263 passed, 14 failed, 10
  ignored. Not one failure names a band, a declared cost or a budget note:
  `budget` (the shim's account against the program's loss; a preempted hook
  call), `cases::every_case_names_its_known_cause_both_ways` (a cited number
  82 ms off against a 77 ms allowance, and an open finding that now passes),
  `family_sched` migrations, `recorder` burst value, `runner` ring loss,
  `scenario_io` rewound counters (`nvme0n1` appeared during the run),
  `scenario_mem` ledger, `scenario_proc` nine sleeps of ten,
  `scenario_sched` round trips, and four in `triage`. Rerun alone, once:
  `scenario_proc` and the three named `triage` tests pass; `budget` passes
  22 of 22 in its first run alone and fails two other tests in its second,
  both timing measurements. The only decisions the code takes on a declared
  band are `--budget`'s ladder and its advance note
  (`src/main.rs::rungs`, `budget_note`), and the test files that drive
  `--budget` (`budget`, `recorder`, `scenario_power`, `triage`) pass alone.
  Reasoned from that, not shown on a quiet machine.
- **While iterating**: `bench_bands` (5), `catalog` unit tests (59),
  `catalog_model` (19), `cli` (26), `budget` (22), `recorder` (8), `runner`
  (28), `scenario_power` (17): green.
