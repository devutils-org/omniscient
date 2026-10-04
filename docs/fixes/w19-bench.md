# The nineteenth wave, stream bench

Branch `feat/w19-bench`, from `main` at `91a0cef`. `BRIEF.md`, "bench -- what
a GPU tracer costs (phase B)". Everything measured here was measured on the
development machine (GeForce RTX 2080 Ti, driver 615.71.09, 24 cpus) with the
other streams of the wave building and testing beside it.

## What landed

- **`examples/bench/gpu.c`**: the GPU workloads, one per regime --
  `launches 50000` (launch-bound, a kernel every 2 us), `compute 50 200000`
  (compute-bound, 1.6 ms kernels), `memory 200 64` (memory-bound, with a
  memset and two copies). Linked against `libcuda.so.1` by name, so the
  preload shim sees its calls; PTX in the file, so no toolkit. Each reports
  `init_ns` (`cuInit` to the module loaded), `elapsed_ns` (the timed
  section), `section_cpu_ns` (the process's cpu over it, every thread) and
  `exit_ns` (the end of `main` to an exit handler registered before `cuInit`,
  so it measures a tracer's last flush). 500 ms of spin kernels before the
  timed section warm the clocks, which are never set.
- **`examples/bench/gpu.rs`**: the arms -- bare; the **negative control**
  (`CUDA_INJECTION64_PATH` naming the measured binary's own
  `libomn_cupti.so`, `OMN_CUPTI_REPORT=0`); served (`omn trace ... --backend
  force:...`); `nsys profile --trace=cuda` as a reference reader, never
  served. The GPU plan (every `gpu` source with a collector serving a
  launch, from the catalog: the two tracers and the five NVML polls), the
  rule (`judged_cost`), the rows, the table's own failures, and the device's
  state read by `nvidia-smi` before and after.
- **`tests/bench_gpu.rs`**: the measurement (`#[ignore]`, about 15 minutes),
  writing `table.md`, `table.json` (with every repetition) and `record.md`
  under `target/bench/gpu-<stamp>/`; and eleven tests in `make check` that
  measure nothing: the GPU record in `docs/bench.md` held to the catalog
  (declared band, worst regime within it or a `stands` reason, no stale
  reason, every regime present, row count, coverage of every launch-only
  `gpu` pair with a collector, `pending: <reason>` for the ones not yet
  measurable), each shown failing on a synthetic record; the rule; the
  IQR-based floor; the fixture compiling without a driver (`-fsyntax-only`).
- **`examples/bench/harness.rs`**: `harness::on_gpu_workloads`, and `plan`
  now leaves `gpu.kernel` and `gpu.api` to the GPU bench with that reason
  (they would be measured on workloads that never call the driver); a few
  items made `pub` for `gpu.rs`.
- **`catalog/gpu.yaml`**, the cost lines: `gpu.kernel` through
  `vendor:cupti` `med` to **`extreme`** (and `vendor:rocprof`, `vendor:l0`
  with it, the equivalent-backend rule); `gpu.api` through `preload` `low`
  to **`high`**. A comment beside each cost line, and **one caveat appended
  to each of the two entries** naming the regime the band is set on (see
  "Outside my row" below).
- **`docs/bench.md`**: "The GPU tracers, nineteenth wave" (workloads, arms,
  rule, both tables, what they say, the bands, the record between
  `gpu-bands` markers); three rows in "What is where"; the `gpu.api` row
  taken out of the backends record (`rows=95` to `rows=94`, with the reason
  beside the marker).
- **`docs/families/gpu.md`**: a section appended, the cost per regime.

### Outside my row, flagged

- `catalog/gpu.yaml`: besides the cost lines, one caveat appended to
  `gpu.kernel` (cupti's entry) and one to `gpu.api` (no stream's this wave).
  The brief asks for "the regime named where the cost depends on it", and a
  YAML comment is not where a reader of `--budget` looks. Additive; no
  existing caveat changed.
- `CATALOG.md` lines 460-461: the cost cells of `gpu.kernel` and `gpu.api`,
  to match the cost lines.
- `examples/bench/harness.rs` is this stream's (`examples/bench/**`), but its
  `plan` change alters what the next backends table measures: two fewer
  pairs, said in its "not measured" list.

## Shown versus reasoned

**Shown** (the record, `target/bench/gpu-1791099533/`: 20 repetitions
round-robin, release build, the workload's threads placed apart, load 13
falling to 1; floors 0.57 / 0.73 / 0.14 %):

- `gpu.kernel` through CUPTI: **+67.05 %** on the launch-bound program
  against the control, the fastest repetitions too (98 ms bare, 161 ms
  armed); process cpu +82.75 %, the band's figure. Compute- and
  memory-bound: +0.81 % and +0.14 % (under 1 %). Exit flush 0.1-0.2 ms.
- **The launching thread pays**: with every other thread kept off its cpu
  (checked on every run), its own cpu grew 66.04 %, about 65 ms, 1.3 us a
  launch; every other thread together spent 16.9 ms more, on other cpus.
  The first round's single-cpu placement had billed those 17 ms to the wall
  clock (+74 to +78 % then).
- `omn`'s own cpu decoding CUPTI's records: 67 ms for 53 000 records, about
  **1.3 us a record** (7.8 % of the launch-bound target's cpu).
- The control against bare: inside the floor on every regime, timed
  section, process cpu and launching thread; the library checked mapped in
  every control and CUPTI run, absent in every bare one.
- `gpu.api` through the shim: +11.31 % launch-bound, on the launching thread
  alone (+11.29 %, other threads +0.0 ms); +3.18 % compute-bound, nothing
  memory-bound. The first round's +9.6 / +38.7 % disagreement is settled
  inside `high`; its `stands` row is gone.
- `nsys --trace=cuda`: +74.71 % launch-bound, its launching thread +74.40 %
  and other threads +48 ms; under the floor elsewhere; +77 to +124 ms at
  `cuInit`, +4.9 to +14.9 ms at exit.
- The NVML polls on GPU workloads: within the bands the backends table gave
  them, on every regime.
- Losses: one late tick each for `gpu.freq` and `gpu.kernel` in twenty
  launch-bound runs (1 of 185, 1 of 1 060 872), nothing elsewhere.
- The clocks: a `compute` section ran 78 ms warm, 119 ms after an idle
  second; 200 ms of warming after three idle seconds left 100 ms, 500 ms
  79-80 ms.
- The record and table checks: each fault shown on a synthetic record or
  table (`bench_gpu`, 11 passed).

**Reasoned, not shown**:

- That what the launching thread pays is CUPTI's activity instrumentation
  and not this library's code: the library registers no per-launch callback
  (three activity kinds and the two buffer callbacks, `cupti/src/lib.rs`),
  and `nsys`, CUPTI underneath, costs that thread about as much. Not
  isolated by disabling activity kinds one at a time.
- Arming CUPTI at `cuInit` (the `init` column): not resolved -- `cuInit`'s
  own spread is tens of milliseconds between runs.
- That one band per backend is the worst regime: a choice consistent with
  `worst_case_percent`, not a measurement. Its consequence: `--budget` under
  20 % refuses `gpu.kernel` on a program where it costs 1 %.
- That an unpinned real program pays what the placed workload pays: the
  launching thread had a cpu to itself here; on a program whose threads
  compete for cpus, CUPTI's other threads' 17 ms can become wall time too.
  The band holds the cpu figure (+82.75 %), which covers that case.

## What I could not do, and why

- **A table with the machine alone.** The record was taken while the other
  streams finished (load 13 falling to 1); its floors (0.14 to 0.73 %) are a
  tenth of the first round's, and no row is near its band's ceiling, so the
  coordinator's run alone is a confirmation rather than a correction.
- **`gpu.counters` and `gpu.profile`**: their collectors are the profiler
  stream's and had not landed. They are `pending` in the record; once they
  land, the same test measures them with no change (its plan is the
  catalog's). If profiler names the backend other than `vendor:cupti`, the
  coverage test names the pair and the pending rows need renaming.
- **The variance gate** is not applied to the GPU table: on the shared
  machine of the first round it refused every time. The table carries its floors instead,
  and the rule reads a dilation under them as zero.
- **`scripts/bench.sh`**: not my file; the GPU table has no step there yet
  (request below).

## Privileged runs wanted

None needs root. One wants the machine and the device alone, after the merge
of this stream and of profiler:

```console
cd /home/moha/repos/omniscient && make release && \
  nvidia-smi --query-compute-apps=pid,process_name --format=csv && \
  OMN_BENCH_BIN=$PWD/target/release/omn cargo test --test bench_gpu \
    every_gpu_tracer_is_costed_against_a_negative_control -- --ignored --exact --nocapture
```

about 15 minutes; then paste `target/bench/gpu-<stamp>/record.md` over the
`gpu-bands` block of `docs/bench.md`, set `was`/`declared`/verdicts, and run
`cargo test --test bench_gpu`. It confirms the record with the machine
alone and measures `gpu.counters` and `gpu.profile`.

## Decision lines for DECISIONS.md

- **A GPU workload's threads are placed apart** (wave 19): the launching
  thread alone on one cpu for the timed section, every other thread (the
  driver's, a tracer's) on three others, and the table reports the
  launching thread's cpu and the other threads' apart. Reason: on one
  shared cpu, a tracer thread's work is the launching thread's wall time,
  and the table cannot say which thread a cost lands on (found in review:
  the first round's +74 to +78 % for CUPTI was +67 % with the threads
  apart).
- **A GPU row that loses more than one record in a hundred is refused**
  (wave 19), by the measurement and by the record's test; every loss is in
  the record as `<lost> of <seen>`. Reason: a tracer that drops records
  skips the work they cost, and under one in a hundred the understatement
  is below every regime's floor.

- **A GPU tracer is costed on GPU workloads, per regime** (wave 19): a `gpu`
  source served on a launched program alone is left out of the backends
  table and measured on a launch-bound, a compute-bound and a memory-bound
  program that call the driver. Reason: on the reference workloads, which
  make no GPU call, a tracer has nothing to trace and reads as free (the
  second table priced the shim at 0.14 %); and the cost of a per-launch
  tracer moves by two orders of magnitude with the regime.
- **CUPTI is costed against a negative control** (wave 19): the injection
  library loaded by the driver and disarmed; a `vendor:cupti` row is armed
  against control, the control against bare is reported apart. Reason: the
  loader's cost is not the tracer's (measured: the control is inside the
  floor).
- **A GPU band is its worst regime, and the regime is named in a caveat**
  (wave 19): `gpu.kernel` `extreme` (+67 % of the time and +83 % of the
  cpu launch-bound, under 1 % otherwise), `gpu.api` `high` (+11.3 %). Reason: a band is a worst case
  (`worst_case_percent`) and the catalog has one per backend; a cost per
  regime would be a schema change, left open.
- **The GPU bench judges a row on the largest of three columns**, the share
  of the target's cpu, the time dilation and the process-cpu dilation, and
  reads a dilation under the workload's floor as zero (wave 19). Reason: a
  tracer inside the target can cost it cpu without stretching its time, and
  `cost.json` cannot see inside the target.
- **The GPU floor is read from the IQR** (wave 19): `IQR / 1.349` in place
  of the standard deviation in the gate's resolution. Reason: on a shared
  machine one held-up repetition moved the standard-deviation floor to 24 %
  on a series whose IQR was 1.4 %; the medians compared are robust to it.
- **The device's clocks are warmed, never set, before a GPU timing**
  (wave 19): 500 ms of spin kernels in the workload. Reason: unlocked clocks
  follow load; an idle second slowed a 78 ms section to 119 ms, which a
  tracer with a slow start would be billed for.

## Needs from another stream

- **cupti**: (1) `omn`'s decoding costs about 1.3 us a record in a release
  build (67 ms for 53 000 records), as much as CUPTI's own 1.3 us a launch
  inside the program: worth a profile of `kernel.rs`'s read path. (2) The
  per-launch price is CUPTI's concurrent-kernel activity; whether a lighter
  kind (or a sampling of launches) is worth offering as a degraded fallback
  is a question for the chain.
- **profiler**: `gpu.counters` and `gpu.profile` are `pending` in the GPU
  record under backend `vendor:cupti`. If your backend id differs, rename the
  two pending rows in `docs/bench.md` (or tell the coordinator); the
  coverage test `every_gpu_tracer_served_on_a_launch_alone_is_in_the_record`
  names the pair otherwise. Your cost lines are this stream's by the table,
  but unmeasured: keep the band you declare, and the quiet run sets it.
- **Coordinator, `scripts/bench.sh`**: a `gpu` step running
  `cargo test --test bench_gpu every_gpu_tracer_is_costed_against_a_negative_control -- --ignored --exact --test-threads=1 --nocapture`,
  in the default sequence after `backends`.
- **Coordinator, the catalog's schema** (open): a cost per regime, or a
  `cost_note`, so that `--budget` need not refuse `gpu.kernel` on a program
  of long kernels. Not proposed as a change here.
- **Coordinator**: the decision lines above; `STATE.md`'s GPU cost rows;
  `CATALOG.md`'s other GPU cost cells (NVML) are stale since the eighteenth
  wave and were not touched.

## The suite

**Fix round**: `make check` at a load of 1 to 3 went red on one test,
`runner::a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed`
(the known gap filed for stream profile, below). `cargo test --workspace
--no-fail-fast` then ran 2375 passed, 2 failed, 12 ignored: that one, and
`unwind::user_only_stacks_carry_no_kernel_frame...`, which passed rerun
alone once. Clippy on every target with `-D warnings` and `cargo fmt
--check` clean. The GPU measurement passed (`gpu-1791099533`, 711 s).

**First round**:

While iterating: `cargo test --test bench_gpu` (11 passed, 1 ignored),
`--test bench_bands` (8), `--test catalog_model` (19); clippy on the bench
tests with `-D warnings`, fmt.

`make check` went red in the lib binary on two perf-sampling tests
(`cpu::profile::...finds_this_thread...`, `mem::page_faults::...sampled_in_user_mode...`)
at a load of about 40. `cargo test --workspace --no-fail-fast` then ran 2365
passed, 11 failed, 12 ignored, at loads of 35 to 54:

- passed rerun alone once: `mem::page_faults::sampled_in_user_mode...`,
  `scenario_net::every_datagram_sent_to_a_closed_port...`,
  `triage::a_verdict_read_off_a_lossy_profile...`,
  `unwind::fp_leaf_recovers...`, `unwind::dwarf_recovers...`;
- failed rerun once, passed on a second rerun when the load fell (they are
  timing-sensitive and touch nothing here): `triage::an_input_this_machine_cannot_serve...`,
  `triage::a_program_whose_work_is_in_another_thread...`,
  `join_engine::a_program_s_life_is_a_whole...`,
  `runner::a_source_attached_for_one_phase...`;
- `runner::a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed`:
  the known gap filed for the profile stream (`docs/open-findings.md`), not
  rerun;
- `cases::every_case_names_its_known_cause_both_ways`: fails twice, on
  `gcc-shell`'s `on-cpu` against gcc's own `-ftime-report` (6.7 s and 7.6 s
  against 9.0 s, beyond the 0.28-0.37 s the resolutions allow) at loads of
  20-50. Nothing in this branch touches a cpu source, the cases or the
  runner; the coordinator's quiet `make check` is the one to read it. (One
  rerun in between failed on a full `/tmp`: about 90 dead test processes'
  `/tmp/omn-cli-<pid>-no-shim` directories, 154 MB each, had filled the
  per-user quota. I removed the ones whose pid was gone; the `cli` tests
  leak that directory -- a finding for whoever owns `tests/cli.rs`.)

## Review (fix round)

The adversarial review's six points, each fixed:

1. **Every thread pinned to one cpu** (defect, `examples/bench/gpu.rs`,
   `gpu.c`). Right: the main thread was pinned before `cuInit`, every later
   thread inherited the one cpu, and "the launching thread pays" rested on
   it. Fixed by placing the threads apart (`GPU_CPUS = 4`, the main thread
   moved alone onto the first cpu after the warm phase, every thread
   `cuInit` created on the other three), reporting the launching thread's
   cpu (`CLOCK_THREAD_CPUTIME_ID`) beside the process's, and failing the
   measurement if any run let another thread onto the launching thread's
   cpu. Re-measured (`gpu-1791099533`): the launching thread still pays
   (+66 % of its own cpu), and the other threads' 17 ms, billed to the wall
   clock before, are now their own column; CUPTI's dilation is +67 %, not
   +74-78 %. The claim is now shown; what it is attributed to (CUPTI, not
   this library) stays reasoned, from the library's code and from `nsys`.
   `docs/bench.md` says how the threads are placed and why the first round's
   two tables are superseded.
2. **`gpu.api`'s caveat on an untracked table** (gap). The record is now the
   rerun, which settles the row inside `high` (+11.31 %); the caveat and the
   cost-line comment quote the record (`gpu-1791099533`), and the `stands`
   row is gone.
3. **Losses and unplanned pairs not checked** (gap, `tests/bench_gpu.rs`).
   The record carries a `losses` column (`<lost> of <seen>`, over every
   repetition), the table the losses by counter; a row losing more than one
   in a hundred fails the measurement and the record's test, and a row with
   no count fails the record's. A pair the tool accepts here whose trial run
   fails now fails the measurement (`Unplanned::failed_its_trial`), as does a
   CUPTI row with no control. The first rerun was refused on one lost
   record in a million, which is why the threshold is a share and not zero.
4. **The control's library never checked loaded** (nit). `gpu.c` reports
   whether the library `CUDA_INJECTION64_PATH` names is in `/proc/self/maps`
   after `cuInit`; the measurement fails unless every control and CUPTI run
   loaded it and no bare run did. The `init` column is still put down to
   `cuInit`'s spread, not measured: said so as reasoned.
5. **Stale doc comment, `within` with a moved band** (nit). The comment
   reads the band from the catalog; `band_faults` names a `within` whose
   `was` differs from its `declared`.
6. **The gpu.kernel caveat's unqualified "nothing measurable"** (nit). It
   now says the control was inside what the bench resolves (0.14 to 0.73 %),
   on every regime; the launch-bound floor is 0.57 %, and the control
   measured -0.39 % there.

Outside my row, still: the two caveats in `catalog/gpu.yaml` were reworded
to the record's figures (no band changed in this round), and `CATALOG.md`'s
cost cells are unchanged since the first round.
