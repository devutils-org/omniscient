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
  under `target/bench/gpu-<stamp>/`; and ten tests in `make check` that
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

**Shown** (two tables, 20 repetitions round-robin, release build;
`target/bench/gpu-1791094863/` at a load of 8, `target/bench/gpu-1791095811/`
at a load of 41-54, the record):

- `gpu.kernel` through CUPTI: **+74 % / +78 %** on the launch-bound program
  against the control, the fastest repetitions too (101 ms bare, 174.5 ms
  armed); cpu dilation equal to time dilation, so the launching thread pays
  (about 1.5 us a launch), not the flush thread. Compute- and memory-bound:
  under the floor (+0.4 to +0.6 %, -3 to -0.9 %). Exit flush 0.1-0.3 ms.
- `omn`'s own cpu decoding CUPTI's records: 91 ms for 53 000 records, about
  **1.7 us a record** (9-10 % of the launch-bound target's cpu).
- The control against bare: inside the floor on every regime, timed section
  and cpu. Loading the library disarmed costs nothing these tables resolve.
- `gpu.api` through the shim: 7.6 / 8.9 % of the launch-bound target's cpu;
  about 1 % elsewhere.
- `nsys --trace=cuda`: +112 / +125 % launch-bound, under the floor elsewhere,
  +105 to +261 ms at `cuInit`, +11 to +27 ms at exit.
- The NVML polls on GPU workloads: within the bands the backends table gave
  them, on every regime.
- The clocks: a `compute` section ran 78 ms warm, 119 ms after an idle
  second; 200 ms of warming after three idle seconds left 100 ms, 500 ms
  79-80 ms.
- The record checks: each fault shown on a synthetic record (`bench_gpu`, 10
  passed).

**Reasoned, not shown**:

- That the CUPTI cost is CUPTI's per-launch instrumentation and not this
  library's: inferred from cpu dilation = time dilation and from `nsys`
  costing more on the same program. Not isolated by disabling activity kinds
  one at a time.
- `gpu.api`'s launch-bound dilation: +9.6 % at a load of 8, +38.7 % at 41.
  I read the second as preemption (the bare launch-bound series spans 101 to
  541 ms there) and the band `high` stands on the first; only a quiet run
  shows it. The record says so in the row's `stands` reason.
- Arming CUPTI at `cuInit` (the `init` column): not resolved -- `cuInit`'s
  own spread is tens to hundreds of milliseconds between runs.
- That one band per backend is the worst regime: a choice consistent with
  `worst_case_percent`, not a measurement. Its consequence: `--budget` under
  20 % refuses `gpu.kernel` on a program where it costs 1 %.

## What I could not do, and why

- **A quiet table.** Both were taken with the wave's other streams running
  (loads 8 and 41-54 on 24 cpus, the device shared in between). The
  floors are wide on the launch-bound regime (13.6 % in the record). The
  coordinator's run alone is below.
- **`gpu.counters` and `gpu.profile`**: their collectors are the profiler
  stream's and had not landed. They are `pending` in the record; once they
  land, the same test measures them with no change (its plan is the
  catalog's). If profiler names the backend other than `vendor:cupti`, the
  coverage test names the pair and the pending rows need renaming.
- **The variance gate** is not applied to the GPU table: on this shared
  machine it would refuse every time. The table carries its floors instead,
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
`cargo test --test bench_gpu`. It settles `gpu.api`'s launch-bound dilation
(the `stands` row) and measures `gpu.counters` and `gpu.profile`.

## Decision lines for DECISIONS.md

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
  (wave 19): `gpu.kernel` `extreme` (+74-78 % launch-bound, under 1 %
  otherwise), `gpu.api` `high`. Reason: a band is a worst case
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

- **cupti**: (1) `omn`'s decoding costs about 1.7 us a record in a release
  build (91 ms for 53 000 records), more than CUPTI's own 1.5 us a launch
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

While iterating: `cargo test --test bench_gpu` (10 passed, 1 ignored),
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
