# The twenty-fourth wave, stream cpu

Branch `feat/w24-cpu`. The defect first (sysstat 2), then the features in the
order they landed. Every file touched is the stream's own (`catalog/cpu.yaml`,
`src/collect/cpu/`, `tests/family_cpu.rs`, `tests/scenario_cpu.rs`,
`docs/scenarios/cpu.md`); no shared layer, no seam. `main` was merged once
(after stream power, run, triage, lazy), cleanly.

## What landed

1. **Defect: a poll's leading event of a few ms** (sysstat 2), `c13eb51`.
   `cpu.util` and `cpu.pressure` took their baseline in `start`, and the
   runner samples right after it, so the first event was a share of a few
   microseconds to milliseconds. Now `start` only reads the file (to refuse
   an offline cpu or a kernel without PSI before anything launches) and the
   first sample is the baseline: no event, not counted as an unmoved read.
2. **`cpu.pressure`'s attributes named as its siblings'** (sysstat 3), same
   commit. The collector reads through the shared `crate::collect::psi`
   reader, as `io.pressure` does: `stall_ratio` (value attribute in poll),
   `stalled_ns`, `interval_ns`, `full_stall_ratio`, `full_stalled_ns`,
   `some_avg10/60/300`, `full_avg*`, `basis`; a snapshot's value is
   `some_avg10`. A counter that goes backwards is a counted loss. The old
   `some`, `avg10`, `total_us`, `full_total_us`, `window` are gone.
3. **`cpu.branches`** (perfstat 1), `19119d9`:
   `PERF_COUNT_HW_BRANCH_INSTRUCTIONS`, a row in `counters::SPECS` and an entry
   in the catalog; count, poll and sample.
4. **Task-clock as a source** (perfstat 4), same commit: `cpu.oncpu_time -m
   count` resolves to a new `pmu:sw` backend (`PERF_COUNT_SW_TASK_CLOCK`,
   modes `[count]`, targets pid/tid/cgroup/launch, degraded: one total read
   when the window closes). Appended last in the chain, so no existing
   resolution moves.
5. **The per-state cpu split** (top 7), `d23ab9d`: `cpu.util` puts the
   window's ticks per state in the results' `fields` (`user_ticks`,
   `nice_ticks`, `system_ticks`, `idle_ticks`, `iowait_ticks`, `irq_ticks`,
   `softirq_ticks`, `steal_ticks`, `ticks`; `cpu<N>.` prefixed per core for
   `--cpu`), and the shares in a fact (`note:` in text). From the baseline
   and the last read, so exact rather than a mean of per-tick shares.
6. **A cycles profile and sampling any PMU event** (perfrecord 2 and 8),
   `dd775cb`. The profile collector is generalised over the event it samples
   (`profile::SAMPLED`): every counting source of the family that declares
   `sample` -- `cpu.cycles` (`pmu` cycles, `pmu:sw` cpu-clock),
   `cpu.instructions`, `cpu.branches`, `cpu.branch_misses`,
   `cpu.cache_misses`, `cpu.context_switches` -- now samples on its own
   counter, with stacks, through the same rings, unwinders and symbols as
   `cpu.profile`. `cpu.cycles -m sample@997hz -e stack` is `perf record`'s
   default. Cycles sample at a fixed period (the cpu's highest frequency over
   the rate; the kernel's frequency mode where that is not published); the
   others in the kernel's frequency mode, each sample weighing one, which a
   note says. Events are of the source's own kind and collector id
   (`cpu.cycles/pmu`).
7. **`pid` and `comm` on every `cpu.profile` sample**, same commit: `pid` is
   the sample's process; `comm` is read per sampled task at a bounded rate
   (`collect::task::Comms`). This also answers perfrecord 1 at the source: a
   `comm` filter on `cpu.profile` now keeps the program's samples instead of
   none with `unseen_fields: ["comm"]`.
8. `docs/scenarios/cpu.md`: defect 11 (the leading event, fixed), and three
   scenario sections (sampling on a counter, task-clock, the per-state split);
   the "not built" list says why `cpu.profile pmu` stays unbuilt.

## Shown vs reasoned

All on this machine, under the wave's load.

- **Leading event** (shown): the reference suite's own probe
  (`poll@50ms`, `-d 175ms`, 40 runs per source) -- `cpu.util` 0 leading
  events of 40, `cpu.pressure` 0 of 40, `mem.pressure` 0 of 40; four events
  every run, the first 50.2-50.5 ms after the window opened. `-e raw` and
  `-e summary,raw` over `-d 3s` both give 3 events (they differed before).
  `cargo test --test reference poll_leading_event` now fails with
  "cpu.util: no leading event in forty runs; defect 2 looks fixed" -- the
  hold test's own signal that the defect is gone (see "Needs").
- **Pairs through the runner** (shown, one run each): `psi-cpu-some` held,
  omn 0.05282 vs `/proc/pressure/cpu` 0.05275 (allowed 0.02) under the new
  names; `vmstat-busy` +0.0023, `mpstat-all` +0.0025, `iostat-c` +0.0015,
  `sar-u` -0.0020 (each allowed 0.03).
- **`cpu.branches`** (shown): `branches_match_a_loop_with_one_branch_per_iteration`
  (50 M iterations of a one-branch loop, within 1 %); through the binary,
  250 416 853 on a C loop of 2.5 branches per iteration x 1e8 (the
  disassembly: `je`, `je`, and a `jmp` on odd iterations), `perf stat -e
  branches` 250 474 530 (`branches:u` 250 022 664).
- **Task-clock** (shown): `task_clock_counts_the_cpu_time_the_thread_ran`
  within 1 % of `CLOCK_THREAD_CPUTIME_ID` over a 200 ms spin; through the
  binary against the same run's reap `cpu_ns`: 716.6 vs 696.8 ms, 485.8 vs
  483.8 ms, 390.0 vs 382.7 ms with two children (0.4 to 2.9 % above, the
  direction `docs/fixes/w23-perfstat.md` saw for perf's own task-clock).
  `perf stat -e task-clock` in separate runs read 844, 822 and 432 ms -- not
  the same run, so not a comparison.
- **Per-state split** (shown): alongside `mpstat 4 1` over the same 4 s,
  twice: user 32.5 / system 9.0 / iowait 7.4 / idle 50.3 % vs mpstat 32.90 /
  9.27 / 7.39 / 49.70; and 41.5 / 5.3 / 7.1 / 45.5 vs 41.39 / 5.28 / 7.06 /
  45.65.
- **Cycles profile** (shown): `a_cycles_profile_samples_one_period_of_cycles_at_a_time`
  -- samples x period equal a cycles counter over the same spin within 10 %
  (+3 samples). On the perfrecord `hot` workload (`omn trace cpu.cycles -m
  sample@997hz -e stack -- perfrecord-hot 4`, 2122 samples, one per 4 687 886
  cycles) the self shares were `hot_beta` 0.495, `hot_gamma` 0.291,
  `hot_alpha` 0.200; `perf record -e cycles -F 997 -p` on another run of it:
  0.483, 0.306, 0.206.
- **Any PMU event** (shown, loosely): `cpu.branch_misses -m sample@997hz -e
  stack` on the same workload, 554 samples: `task_tick_fair` 0.081,
  `hot_beta` 0.079, `hot_gamma` 0.061, `native_read_msr` 0.060; `perf record
  -e branch-misses -F 997`: `hot_beta` 10.1 %, `hot_gamma` 6.9 %,
  `hot_alpha` 5.8 %, `task_tick_fair` 5.8 %. Same functions at the top,
  shares a few points apart: perf weighs each sample by its period, omn
  cannot see the period (see "Not done").
- **`comm` on samples** (shown): `cpu.profile ... --filter 'comm ==
  "perfrecord-hot"' -- perfrecord-hot 1` kept 998 of 998 (was 0 of 997,
  `unseen_fields: ["comm"]`); `a_profile_filtered_on_the_programs_name_keeps_its_samples`.
- **Reasoned only**: the fixed cycles period assumes `cpuinfo_max_freq` of
  cpu0 is every core's highest (true here; a hybrid machine's E-cores have a
  lower one, which makes their samples rarer per second but still one per
  the same number of cycles). The frequency-mode bias of the non-cycles
  events is described, not measured.

## Tests run

- `cargo test --lib collect::cpu` -- 90 passed (new: two in `pressure.rs`
  beyond the rewritten ones, two in `util.rs`, two in `counters.rs`, two in
  `profile.rs`).
- `cargo test --lib catalog` -- 61 passed.
- `cargo test --test family_cpu` -- 19 passed (new:
  `counted_oncpu_time_is_task_clock_and_agrees_with_the_reap`,
  `a_counting_source_samples_on_its_own_counter`,
  `a_profile_filtered_on_the_programs_name_keeps_its_samples`; the two
  counting sources added to `every_counting_source_produces_one_value...`;
  `sampling_a_counter_is_refused_with_the_mechanism_it_would_need` removed,
  since that request is now served).
- `cargo test --test scenario_cpu` -- 42 passed
  (`a_mode_the_build_does_not_implement_exits_as_unimplemented` now asks
  `mem.cache_l1 -m sample`, the one counting source left without a sampler).
- `cargo test --test reference` -- `pairs_sysstat` for the five pairs above,
  held. (Before the fix round, `poll_leading_event_is_held_as_a_known_defect`
  failed as its own signal that the defect was fixed; see "Review".)
- `cargo test --test cli the_catalog_document_s_cost_column` -- passed.
- `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt
  --check` -- clean. No `make check`, no `cargo test --workspace`.

## Not done, and why

Ranked.

1. **`func` and `object` keys on `cpu.profile`** (perfrecord 3-4). Stream emit
   derives `func` from each event's stack (its `7552e22`, not on `main` when
   this was written); the source offers it by listing `func` in its keys, and
   that line cannot land before `KeyKind::Func` does -- the loader would
   refuse the catalog. The line, for whoever merges both:
   `keys: [task, cgroup, cpu, func]` on `cpu.profile` (and on the counting
   sources that sample, `cpu.cycles` first, if emit's derivation tolerates
   events without stacks as `func:?`). `object` is emit's to derive the same
   way, from the leaf frame's module.
2. **Instruction attribution** (perfrecord 7). Every sample already carries
   its `ip`; what is missing is a key over it -- `insn:<func>+0x<off>` --
   derived like `func`, which needs the symbol's start address in the frame
   (`src/stack.rs`, stream unwind) and the key (stream emit). Not attempted.
3. **A sample's period for the frequency-mode events.** Sampled
   `cpu.instructions`, `cpu.branches`, `cpu.branch_misses`,
   `cpu.cache_misses` and `cpu.context_switches` weigh each sample one, where
   `perf report` weighs it by its period. `PERF_SAMPLE_PERIOD` in
   `src/perf.rs`'s `SAMPLE_TYPE` and the decoder (a shared layer) would let
   the collector attach `weight`; then the shares are period-weighted and
   `-e summary` over a sampled counter estimates its count.
4. **`cpu.profile`'s `pmu` (cycles) row stays unbuilt**, deliberately: making
   it the chain's first served backend would turn every `cpu.profile` into a
   cycles profile, and `omn latency` (`src/triage/latency.rs`) and `omn diff`
   (`src/triage/diff.rs`) read a `cpu.profile` sample as a period of
   nanoseconds. The cycles profile is `cpu.cycles -m sample`; see the
   decision line.
5. **The per-state split inside `summary`** (top 7, the other half). The
   split is in `fields` and a note; a `summary` series per state is emit's
   "summary per attribute" (emit's `baa6ff3`), which `cpu.util`'s raw events
   already feed (`user`, `system`, `iowait`, `steal` are attributes of every
   event).
6. `mem.cache_l1`, `mem.cache_llc`, `mem.cache_tlb` declare `sample` and still
   refuse it at start (exit 3, "needs the sampling path"): a cache descriptor
   is `PERF_TYPE_HW_CACHE`, and a row in `profile::SAMPLED` per descriptor is
   all it takes, but those entries are the mem stream's catalog.

## Privileged runs wanted

None beyond `sudo -n perf` (granted), used for the `perf stat` and `perf
record` comparisons above. For the record, the exact commands:

```console
$ sudo -n perf stat -e branches:u,branches -x, -- ./br 100000000
$ sudo -n perf record -q -e cycles -F 997 -p $PID -o perf.data -- sleep 3
$ sudo -n perf report -i perf.data --stdio --no-children -g none --sort sym
$ sudo -n perf record -q -e branch-misses -F 997 -o perf2.data -- target/reference/perfrecord-hot 3
```

## Decision lines

- **A counting source's `sample` mode samples on its own counter**
  (twenty-fourth wave, stream cpu): `cpu.cycles -m sample -e stack` is the
  cycles profile, `perf record -e <event>` is `<source> -m sample`, one
  collector for all (`src/collect/cpu/profile.rs`, `SAMPLED`). Cycles sample
  at a fixed period of cycles; the rest in the kernel's frequency mode, each
  sample weighing one until the period is decoded.
- **`cpu.profile` is the on-cpu time profile**: its `pmu` (cycles) row has no
  collector on purpose, because its samples are periods of on-cpu time that
  `omn latency` and `omn diff` read as nanoseconds. Either the row is removed
  from the chain or it stays as a pointer to `cpu.cycles -m sample`; the
  coordinator's call.
- **Task-clock is `cpu.oncpu_time -m count`** on `pmu:sw`, last in the chain,
  not a source of its own.
- **`cpu.pressure` speaks the PSI reader's names** (`stall_ratio`,
  `some_avg10`, ...), as `mem.pressure` and `io.pressure` do.

## Needs from another stream

### To reference (`tests/reference/sysstat.rs`)

- Done in this stream's fix round (see "Review"). Still open: the
  `summary.mean` pair for `cpu.util` against `mpstat` that
  `docs/open-findings.md` asks for once the leading event is fixed.

### To emit

- `func` lands on `cpu.profile` by one catalog line once `KeyKind::Func` is
  on `main` (above); `object` and an `insn` key are asked the same way.
- `PERF_SAMPLE_PERIOD` (filed to whoever owns `src/perf.rs`), then `weight`
  per sample for the frequency-mode events.

### To the coordinator

- `docs/CATALOG.md`: a `cpu.branches` row; `cpu.oncpu_time` gains `count` and
  `pmu:sw`; `cpu.pressure`'s attribute names; `cpu.cycles` and the counting
  sources' `sample` now served.
- `man/omn.1` and `docs/dev/STATE.md`: sysstat 2 and 3, perfstat 1 and 4, top 7 (the
  `fields` half), perfrecord 1, 2 and 8 done; perfrecord 3-4 with emit;
  perfrecord 7 open.

## Review

The reviewer was right: the branch fixed defect 2 but left
`poll_leading_event_is_held_as_a_known_defect` red and `raw_means` skipping a
real first event, so the coordinator's full suite would have gone red. Fixed
here rather than waiting for the reference stream, in
`tests/reference/sysstat.rs` only (outside the owned set, a test file, not a
shared layer):

- `raw_means` no longer skips a leading event; every raw event is a full
  period and counts.
- The hold test is turned around as `poll_has_no_leading_event`: ten short
  runs (`poll@50ms`, `-d 175ms`) each of `cpu.util`, `cpu.pressure`,
  `mem.pressure` and `io.pressure`, and no event may be stamped within half a
  period of the window's opening.
- `omn-cpu-util-raw.json` and `omn-cpu-pressure-raw.json` recaptured with
  this branch's binary (`-m poll@1s -e raw -d 3s`): three events each, the
  first a second after the window opened, `cpu.pressure` under the new names.
  `omn_cpu_util_leaves_out_the_few_ms_leading_event` is now
  `omn_cpu_util_means_every_raw_event`; `omn_psi_reads_both_namings` is
  `omn_psi_maps_the_pressure_names`, and `omn_psi`'s `("some", "some")`
  mapping, which nothing emits any more, is gone.

Tests: `cargo test --test reference sysstat` -- 17 passed (the parsers,
`poll_has_no_leading_event`, `pairs_sysstat`); `cargo clippy --workspace
--all-targets -- -D warnings` and `cargo fmt --check` clean.
`docs/open-findings.md` still names the old test in two historical entries;
the coordinator's to close.
