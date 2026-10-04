# The nineteenth wave, stream profiler

Branch `feat/w19-profiler`, from `main` at `91a0cef` (cupti, device and core
merged). `BRIEF.md`, "profiler -- inside a kernel (phase B)".

## What landed

- **`cupti/src/profiler.rs`** (new): `gpu.counters` in the injection library.
  CUPTI's range profiler (`cuptiRangeProfiler*`, `cuptiProfilerHost*`) set up
  on every context at its creation callback, in **kernel replay** with
  **automatic ranges**; launches recorded at the exit of their launch call
  (`cuLaunchKernel`, its `_ptsz`, the cooperative and `Ex` forms), the
  counter data image decoded every 32 launches, at context teardown and at
  exit, each range evaluated and paired with its launch by order. Each
  counter of each launch is one `OP_COUNTER`. Ranges dropped, ranges and
  launches that did not pair, launches never decoded: each counted
  (`OP_INSIDE_LOST`, with its kind); since the review, a batch is paired
  only when nothing can have shifted it (see "Review"). Metrics the chip
  does not have are named
  and the others measured. Every CUPTI call's result is checked and named in
  the error. Clocks are never locked (`cuptiClockControlLock` is not called).
- **The witness.** The library always adds `sm__ctas_launched.sum` and
  compares it, per launch, with the grid read from the launch call's own
  parameters: a launch whose window held more blocks than its grid is
  marked disturbed; fewer is a range that is not the launch's, and its
  batch is counted unpaired. See "Found".
- **`cupti/src/pcsampling.rs`** (new): `gpu.profile` in the library. PC
  sampling enabled and configured at context creation, **continuous** mode
  (kernels not serialised), every `2^12` SM cycles (`OMN_CUPTI_PC_PERIOD`),
  all stall reasons; every attribute's own status checked, the applied period
  and mode **read back** and sent beside what was asked (`OP_PC_CONFIG`);
  every module kept by its CUPTI CRC so each program counter is resolved to
  file and line by `cuptiGetSassToSourceCorrelation`; drains every 256
  launches, at module load, teardown and exit (`OP_PC_DRAIN`, then one
  `OP_PC` per program counter and stall reason). CUPTI's function names are
  freed once per function (it shares one allocation between a function's
  program counters: freeing per program counter read freed memory, shown).
- **`cupti/src/wire.rs`**: `REPORT_COUNTERS = 2`, `REPORT_PC_SAMPLING = 4`;
  ops 13-20 (`OP_INSIDE_FIRST..=OP_INSIDE_LAST` = 13-31 reserved for inside
  readers); `CounterSetup`, `Counter`, `PcConfig`, `PcSample`, `PcDrain`;
  `Reader::checked`; the metric defaults and the witness name.
- **`cupti/src/lib.rs`** (cupti's): `InitializeInjection` arms on any non-zero
  report, says hello and calibrates whatever it is armed for, then arms each
  bit's module; refuses counters with PC sampling in one process; at exit
  reads the inside modules back before the activity flush. Activity arming is
  unchanged when its bit is set.
- **`cupti/src/channel.rs`** (cupti's): inside records lost in an unsent
  batch are counted apart (`inside_lost`, `OP_INSIDE_UNSENT`), so a launch
  read by `gpu.kernel` and `gpu.profile` does not bill one reader for the
  other's records. One unit test.
- **`cupti/src/api.rs`** (cupti's): keeps the handle; `Cupti::symbol` for the
  modules that resolve entry points of their own.
- **`src/collect/gpu/counters.rs`** (new): `InsidePrepared` (wraps
  `KernelPrepared`, so the inside readers and `gpu.kernel` share one socket
  whichever was asked first), the refusals at attach, `Common` (the frames
  every inside reader handles, NVML's list of other compute processes once a
  second), and `CountersCollector`: one event per kernel and counter at the
  end of the run -- `value` (median), `min`, `max`, `spread` (interquartile
  range over median), `outliers`, `launches`, `disturbed`, `passes`,
  `verdict`. Nine unit tests.
- **`src/collect/gpu/profile.rs`** (new): `ProfileCollector`, one event per
  program counter and stall reason per drain, weighed by `samples`, stack
  `kernel;file:line;stall` (`stacks()` returns the frames), stamped at the
  drain in the program's `CLOCK_MONOTONIC` with the drain window as
  `ts_err_ns`; `_not_issued` reasons left out (a subset that would count
  samples twice). Three unit tests.
- **`src/collect/gpu/kernel.rs`** (cupti's): `mod wire` and
  `KernelPrepared::new` made `pub(super)`, `reader()` and `Socket::report()`
  added, `close_write` made `pub(super)`, and the inside ops skipped rather
  than counted as undecoded. Flagged so the merge expects it.
- **`src/collect/gpu/mod.rs`** (device's): `mod counters; mod profile;` and
  two `Factory::new` lines.
- **`catalog/gpu.yaml`**: `gpu.counters` rewritten (`vendor:cupti` in place
  of `vendor:ncu`, `trace`, `[raw, topk]`, `{launch}`, `[task, proc]`,
  determinism `medium`, caveats); **`gpu.profile`** new (`vendor:cupti`,
  `trace`, `[stack, topk, raw]`, `{launch}`). The two new `cost:` values are
  placeholders for bench (`extreme`, `med`).
- **`CATALOG.md`** section 8: the two rows and their caveats.
- **`tests/gpu_inside.py`** (new): `hotloop N ITERS` (sixteen dependent FMAs
  on line 20 of `.loc`-annotated PTX, JIT with `CU_JIT_GENERATE_LINE_INFO`),
  `hotloop-bare` (no line information); everything else delegated to
  `tests/gpu_workload.py`, which is untouched.
- **`tests/gpu_counters.rs`** (new): seven scenarios (below).
- **`tests/family_gpu.rs`, `tests/scenario_gpu.rs`** (device's): their
  "no collector" checks named `gpu.counters`; they now pin
  `force:vendor:rocprof`, which still has none. `gpu.profile` added to the
  listing check.
- **`docs/families/gpu.md`**: the section "Inside a kernel".

## Review

The adversarial review's ten points, all taken; none was wrong.

- **Pairing by position (defect)**: fixed. `pairing()` in
  `cupti/src/profiler.rs` pairs a batch by order only when
  `ranges == launches` and nothing was dropped; any other batch is not
  reported and `max(ranges, launches - dropped)` is counted `LOST_UNPAIRED`.
  The witness is a second check: every range is evaluated first, and one
  with **fewer** blocks than its launch's grid marks the batch mispaired
  (`witnesses()`), so it is not reported either. Two unit tests, including
  the reviewer's `[A1, G, A2]` against `[A1, A2]`. No workload here launches
  a graph, so the mismatch is shown by unit test, not end to end; the
  end-to-end runs reported no unpaired batch before or after.
- **Vacuous pass (gap)**: the verdict test now prints a `skipped:` line with
  the device's tenants (`nvidia-smi`'s list) and each attempt's evidence, so
  the skip gate sees it. It asserted on this run.
- **`gpu.profile` held to the self-instrumenting kernel (gap)**: new test
  `the_samples_are_the_kernel_s_own_cycles_over_the_period`: `selftimed 10
  100000` under `gpu.profile` at `2^12` and `2^14`; samples on `selftimed`
  against `sum(max_block_cycles x sms) / 2^n x 11/10` (the workload's
  warm-up launch is the eleventh). Measured 1.0006 and 1.0014; held within
  10 %. This also shows the period is per SM, not per sub-partition.
- **Counter timestamps (gap)**: the library records `CLOCK_MONOTONIC` at the
  entry of each launch call (a thread-local set at `API_ENTER`), carried in
  `wire::Counter::launched_ns` (the frame grew 8 bytes). Each event is
  stamped at its kernel's first launch entry, `ts_err_ns` reaching to the
  arrival of its last counters. `memory 6 64` now reads `ts_err_ns` = 100 ms,
  not 1. Limit: `ts_err_ns` is a `u32` and saturates at 4.29 s, which a
  kernel launched across a longer run reaches; said in the caveat.
- **Sample shortfall (gap)**: `ProfileCollector::account_for_samples` holds
  `cupti - non_user` to `reported`; a shortfall past what CUPTI said it
  dropped is added to `dropped` with the reason (unit test). Whether CUPTI's
  total includes its dropped samples is not documented, so the dropped are
  subtracted: never counted twice, at worst a shortfall smaller than the
  dropped count is hidden behind a loss already counted. The "kernels with no
  sample" claim was wrong as written: one kernel with no sample among others
  is expected (a kernel shorter than the period takes none) and is not a
  loss; the module doc, the catalog caveat and the family document now say
  "a run whose kernels took no sample at all". `channel.rs` counts
  `OP_COUNTER_SETUP`, `OP_STALL`, `OP_PC_CONFIG` and `OP_PC_DRAIN` in an
  unsent batch too (unit test extended). These overlap with the shortfall
  when a drain's account is sent and its program counters are not: two
  counts of one loss in different units, said here rather than reconciled.
- **Witness direction (nit)**: fewer blocks is no longer `disturbed`; it is a
  mispaired batch (above).
- **Double count on a late decode failure (nit)**: `decode_into` reports how
  many launches it accounted for; `decode` counts only the rest as
  undecoded, so a failure in `initialize_image` after the sends counts none.
- **Period ignored silently (nit)**: `period_from` returns the reason, sent
  as a `gpu.profile:` error, so the notes say the setting was ignored and the
  default used (unit test). `LOST_HW_BUFFER_FULL` is now added to
  `throttled` (occurrences whose cost is not knowable), not to `dropped`.
- **Test nits**: the offset test now checks `source` is exactly `+0x<pc>`
  and that the bare kernel's hottest offset is the lined one's, on line 20;
  the pid-refusal test accepts "no viable backend" only where CUPTI does not
  resolve.
- **`CATALOG.md` (nit)**: flagged under "Needs from another stream".

## Shown versus reasoned

- **Shown** (`tests/gpu_counters.rs`, green here on the shared device):
  - **Two workloads that disagree about a counter**: `dram__bytes_read.sum`
    `reproducible here` on the 64 MiB copy (at least 64 MiB every launch),
    `not dominated by this workload` on the compute-bound spin, the first's
    spread at least ten times below the second's; `smsp__inst_executed.sum`
    `reproducible here` and bit for bit on both (3 167 488 and 353 620 672)
    over the launches the witness found undisturbed; every launch either
    measured or counted disturbed.
  - **The known hot loop first**: line 20 holds 95.9 % of the samples, more
    than half asserted; lines 10 and 30 under 1 % of it each; every sample
    on `hotloop.cu`; the facts say 100.0 % on a source line.
  - **A kernel without line information** placed by offset, named in the
    facts.
  - **The period applied, by its effect**: `2^12` took 667 450 samples,
    `2^14` 166 880 (asserted: fewer than half); the facts name the period
    read back.
  - **Refusals**: counters with `gpu.kernel`, counters with `gpu.profile`,
    each with its reason, before the program runs; `--pid` for both.
  - **One launch read by `gpu.kernel` and `gpu.profile`**: three kernels
    exact, samples present, neither reader counting the other's frames.
- **Shown by unit tests**: the verdict rules (repeat, wander, one launch,
  zero median, a few disturbed launches as outliers); a disturbed launch left
  out and counted; losses with their reasons; another reader's errors not in
  this reader's notes; the refusal matrix; sharing a socket with `gpu.kernel`
  either way; the profile's events, stacks, window error and `_not_issued`
  exclusion; a configuration accepted and not applied (period, mode, no
  sample) said; the parameter blocks' sizes and offsets against CUPTI 13.4's
  headers (measured with a C program); the grid read from both launch
  layouts.
- **Shown by hand, not asserted**: the witness's numbers (272 for the copy's
  grid, 892 to 1 032 when another process's work was in the window; 68 and
  688 to 756 for spin); samples reported per program counter adding up to
  CUPTI's count less its non-program samples (2 224 755 + 2 412 =
  2 227 167); NVML missing tenants that live under a second.
- **Reasoned, not measured**:
  - that pairing ranges and launches by order holds under several threads
    launching on one context (one lock serialises the callbacks; not
    exercised with more than one thread);
  - that the witness catches every disturbance that launches blocks: it
    cannot see work already resident in the window (a long kernel started
    before it), which shows only as outliers;
  - that a multi-device program works: every context is keyed by its device,
    unexercised past one;
  - what either source costs (bench's).
- **"Instruction counts reproducible on both"** holds only on the launches
  nobody else's work reached. The test holds it there; when three attempts
  in a row leave fewer than four undisturbed launches of a kernel (it
  happened while the bench stream looped GPU programs: every compute launch
  of three runs disturbed), it skips through the skip gate, naming the
  device's tenants.
- **Since the review, shown**: samples against the self-timing kernel's own
  cycles (1.0006 at `2^12`, 1.0014 at `2^14`); counter events stamped at the
  launches (`ts_err_ns` 100 ms on `memory 6 64`); the bare kernel's hottest
  offset equal to the lined one's. **By unit test only**: a batch with a
  range no launch accounts for is not reported (no workload launches a
  graph); the sample shortfall counted; a period setting ignored and said.

## Found

- **The device counts other processes' work, and a wrong count can repeat.**
  With another worktree's GPU programs running, the copy kernel's
  instruction count read 286 220 778 instead of 3 167 488 on every launch of
  a run -- the same wrong number each time -- so a spread alone called it
  reproducible. NVML's process list did not show the other program. The
  witness (`sm__ctas_launched.sum` against the launch's grid) is the fix;
  it is a decision line below.
- **CUPTI shares one name allocation between a function's program
  counters**: freeing it per program counter read freed memory on the next
  (garbled kernel names, shown); freed once per function now.
- **`_not_issued` stall reasons are a subset**, and counted beside their
  reason they double the samples.
- **NVML's compute process list took up to 95 ms** on this shared device; at
  every tick it made a 200 ms tick late. Asked once a second now, and NVML
  opened at `start`.

## What I could not do, and why

- **A refusal classified as the request's.** The counters-beside-kernel
  refusal is made at attach (`Prepared::attach` returning the reason), the
  only point where every reader's bit is known; the runner labels an attach
  refusal as the machine's (`Whose::Machine`, "cannot start collecting").
  The program never runs (it is killed at its gate). A pre-launch check in
  the validator or a `Prepared` hook would be the request's; not my files.
- **`--emit stack` on `gpu.profile` warns** "no --stack given (or `auto`),
  so the resolver picks the best unwind mode": the warning is for cpu
  unwinding and means nothing here. Not my file (the resolver or validator).
- **The cost bands**: bench's. The two new `cost:` values are placeholders.
- **Counter values per launch** (`raw` of every launch) are not emitted:
  the event is per kernel and counter. A launch-level view would need an
  event per launch and counter; left for when a view asks.
- **`ncu` as a second reader** of an instruction count: present at
  `/usr/bin/ncu`, not used; the two workloads' disagreement and the witness
  were the references.

## Privileged runs wanted

None needs root (`RmProfilingAdminOnly` is 0 here; were it 1, both sources
would fail at their first CUPTI profiling call, with CUPTI's
`CUPTI_ERROR_INSUFFICIENT_PRIVILEGES` in the notes). One wants the device
alone, to see the instruction verdict asserted without any disturbed launch
and the numbers in the family document taken without another tenant:

```console
cd /home/moha/repos/omniscient && make && nvidia-smi --query-compute-apps=pid,process_name --format=csv && cargo test --test gpu_counters -- --nocapture
```

## Decision lines for DECISIONS.md

- **`gpu.counters` is CUPTI's range profiler in kernel replay with
  automatic ranges, from the injection library** (wave 19); `vendor:ncu`
  left the chain. Reason: nothing is wrapped, and kernel replay is what
  gives a counter per launch without the program's cooperation.
- **A counter's verdict is measured per kernel and per run, on the
  interquartile spread over the median of its undisturbed launches, at
  10 %** (wave 19): `reproducible here`, `not dominated by this workload`,
  `measured once`, `disturbed on every launch`; `outliers` beside it.
  Reason: what disturbs a counter on a shared device only adds and reaches
  a few launches; a full-range spread let one disturbed launch turn a count
  that repeats bit for bit into one that does not.
- **Every launch's window is witnessed by `sm__ctas_launched.sum` against
  its grid, and a launch with more blocks is disturbed, counted and left
  out; a range with fewer blocks is not its launch's** (wave 19). Reason: measured, another process's kernels added the
  same 283 million instructions to every launch of a run, so the wrong
  count repeated and no spread could tell; the blocks launched are fixed by
  the launch call, so a difference is other work, by construction.
- **Counters beside `gpu.kernel` or `gpu.profile` are refused** (wave 19),
  at attach, before the program runs. Reason: kernel replay makes kernel
  times those of replayed kernels; CUPTI cannot run the range profiler and
  PC sampling in one process.
- **`gpu.profile` is CUPTI's PC sampling in continuous mode, aggregated per
  program counter and stall reason, resolved to source lines from the
  modules the library keeps** (wave 19). Reason: continuous mode does not
  serialise the program's kernels; the per-PC aggregate is what CUPTI
  produces, so an event per PC and reason per drain loses nothing.
- **A PC sampling configuration is checked by what CUPTI applied and by its
  effect, not by the status code** (wave 19): attribute statuses, the period
  and mode read back, a run with no sample at all a lost read, and CUPTI's
  sample count held to the samples reported.
- **Ranges pair with launches by order only when a batch's counts agree and
  no range has fewer blocks than its launch; any other batch is not
  reported and counted unpaired** (wave 19). Reason: a range carries no
  launch's identity, and one range with no recorded launch (a graph's
  kernel) shifts every later launch onto its neighbour's counters.
- **`_not_issued` stall reasons are left out** (wave 19). Reason: a subset of
  their reason; counted beside it, a sample counts twice.
- **Inside readers share `gpu.kernel`'s socket; ops 13 to 31 are theirs and
  a reader of activity skips them** (wave 19); their unsent records are
  counted apart (`OP_INSIDE_UNSENT`).

## Needs from another stream

- **Coordinator (`CATALOG.md`)**: section 8 was edited by this stream (the
  `gpu.counters` and `gpu.profile` rows and caveats, and since the review
  the pairing and shortfall sentences), although `BRIEF.md`'s ownership
  table does not list `CATALOG.md` for profiler: expect it at the merge.

- **Coordinator / cupti's files**: expect the edits listed above in
  `cupti/src/{lib,api,channel,wire}.rs` and `src/collect/gpu/kernel.rs`
  (visibility, the inside ops skipped). `src/collect/gpu/mod.rs` and
  `tests/{family,scenario}_gpu.rs` (device's): the lines listed above.
- **Coordinator (`src/collect/mod.rs` or `src/validate.rs`)**: a way to
  refuse a combination of requests on one launch as the *request's*
  (exit 2, `Whose::Request`) before the launch -- a `Prepared` method
  answering a refusal after `share_launches`, or a validator rule over the
  session's sources. Today the counters-beside-kernel refusal is an attach
  error labelled the machine's.
- **Coordinator (the resolver's stack warning)**: do not warn about
  unwinding for a source whose stacks are not unwound (`gpu.profile`'s are
  built from program counters and lines). A catalog flag, or the warning
  restricted to the `cpu` family's samplers.
- **bench**: the `vendor:cupti` cost lines of `gpu.counters` (kernel replay:
  each launch run once per pass, one pass for the default set here) and
  `gpu.profile` (period `2^12`), against `OMN_CUPTI_REPORT=0`; `bench.md`
  lines 617-618 name `vendor:ncu`, which left the chain.
- **device (`docs/scenarios/gpu.md`)**: lines 425-428 and 517-522 say
  `gpu.counters` has no collector and name `vendor:ncu`.
- **Coordinator**: the decision lines above; `STATE.md`'s `gpu.counters` row
  and a `gpu.profile` row; `STATE.md` line 978 (`CUPTI, Nsight`).
- **lanes / flame**: `gpu.profile` gives stacks of `kernel;file:line;stall`
  through `Collector::stacks()`, so a flame of the device's cycles needs no
  code of its own.

## The suite

`make check` went red on tests outside these paths
(`cpu::profile::...finds_this_thread...`, `mem::page_faults::...`, `cases`).
`cargo test --workspace --no-fail-fast` then ran 2211 passed, 6 failed, 9
ignored, with `/tmp` over quota at times (other worktrees share it), which
also truncated the log before the GPU binaries:

- `cross_compute::a_wakeup_join_onto_drained_totals_...` and
  `scenario_runtime::without_its_capabilities_...`: "Disk quota exceeded"
  writing to `/tmp`; each passed rerun alone once.
- `unwind::fp_leaf_recovers_...` and `page_faults::sampled_in_user_mode_...`:
  in the flaky tally; each passed rerun alone once.
- `cases::every_case_names_its_known_cause_both_ways`: two cases' on-cpu
  figures against their references under the machine's load
  (`gcc-shell/the-passes-o2-adds`, `cpython-perf-map/one-function-dominates`);
  nothing here touches the cpu sources. Not rerun (minutes, and load-bound).
- `runner::a_profile_s_losses_land_on_the_series_...`: the known gap filed
  for the profile stream.

Rerun after: `gpu_counters` 7 passed, `gpu_kernel` 11, `scenario_gpu` 32,
`family_gpu` 10; `omn-cupti` 8 unit tests, `collect::gpu` 60. Clippy
(`--workspace --all-targets -D warnings`) and fmt clean.

### The suite, after the review round

`make check` went red only on `cases::every_case_names_its_known_cause_both_ways`
(`cpython-perf-map/a-blocking-read` frames, `gcc-shell/the-passes-o2-adds`
on-cpu figure: the cpu sources under load, not touched here). `cargo test
--workspace --no-fail-fast` then ran 2554 passed, 3 failed, 11 ignored --
`cases` passed that time -- with:

- `runner::a_profile_s_losses_land_on_the_series_...`: the known gap filed
  for the profile stream.
- `triage::a_detailed_layer_over_its_budget_is_let_go_...` and
  `unwind::a_stripped_build_is_named_from_debuginfod_...`: each passed rerun
  alone once.

Before it: `gpu_counters` 8 passed (no skip; the verdicts asserted),
`gpu_kernel` 11, `scenario_gpu` 32, `family_gpu` 10, `omn-cupti` 10 unit
tests, `collect::gpu` 61, `catalog` 59, `cli` 26. Clippy and fmt clean.
