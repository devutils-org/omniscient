# The nineteenth wave, stream cupti

Branch `feat/w19-cupti`, from `main` at `4aef377`. `docs/dev/BRIEF.md`, "cupti --
every kernel, its time, and what was lost".

## What landed

- **The injection library**, a new workspace member `cupti/` (package
  `omn-cupti`, built as `libomn_cupti.so`, beside `omn` like the preload
  shim). The CUDA driver loads it from `CUDA_INJECTION64_PATH` inside
  `cuInit` and calls `InitializeInjection`, which:
  - opens CUPTI at runtime (`OMN_CUPTI_LIB`, then `libcupti.so`,
    `libcupti.so.13`, `libcupti.so.12`) -- nothing is linked;
  - enables concurrent-kernel, memcpy and memset activity, hands CUPTI at
    most 256 buffers of 256 KiB (`OMN_CUPTI_BUFFERS`,
    `OMN_CUPTI_BUFFER_BYTES` override them), and answers a request past the
    bound with no buffer, so CUPTI drops and counts;
  - drains on a flush thread every 50 ms (`OMN_CUPTI_FLUSH_MS`), takes a
    calibration bracket (monotonic, CUPTI, monotonic; the tightest of four) at
    each flush and of 32 at start and exit, asks
    `cuptiActivityGetNumDroppedRecords` after every buffer;
  - forces the last flush at exit and sends an `END` frame;
  - `OMN_CUPTI_REPORT=0`: loads and arms nothing, the negative control.
  It answers success from `InitializeInjection` whatever happens and reports
  every CUPTI failure as an `ERROR` frame, which the run prints.
- **The channel** (`cupti/src/wire.rs`, compiled on both sides -- `omn`
  includes it by `#[path]`): one `AF_UNIX` `SOCK_SEQPACKET` socket per launch,
  messages of at most 60 KiB batching frames, the sender's pid in each
  header, kernel names interned per process. Sends are bounded by a 2 s
  timeout; an unsent batch is counted -- its records and the drop and
  unfinished counts it carried -- and reported in the next one, and a name it
  carried is sent again. **This departs from the brief**, which asked for
  records shipped "the way the preload shim ships its own (a shared ring...)":
  CUPTI hands records over a buffer at a time on a thread that is not the
  program's, so a batch per buffer on a socket costs no system call per
  record, needs no shared mapping set up before `cuInit`, and gives a
  bounded wait in place of a ring's overwrite. The decision line below says
  so.
- **`src/collect/gpu/kernel.rs`**: `gpu.kernel` through `vendor:cupti`, for a
  launch target. One event per kernel, memcpy and memset: `duration_ns`,
  `op`, `kernel` (demangled, or `[memcpy HtoD]`...), `device`, `stream`,
  `context`, `correlation`, `grid`, `block`, `registers`, `shared_bytes`,
  `bytes`, `copy`, `value`, `pid`; keys `task` and `proc`. Losses: CUPTI's
  drops, unsent batches and records returned unfinished in `dropped`, each
  with its reason; a process that never sent `END` as a failed read with the
  reason; unknown frames and records before any calibration as `undecoded`.
  Facts: CUPTI's version, the buffers, the brackets and the map's error and
  rate; the activities per op and the widest error. Several CUPTI requests on
  one launch share one socket, fanned out per reader (`Socket`), as the
  shim's pipe is.
- **`src/clock.rs`**: `Calibration`, `Bracket`, `ClockMap` -- the line through
  the first and last brackets' midpoints, error = widest half-bracket + the
  furthest bracket from the line, at least 1 ns. Five unit tests.
- **`catalog/gpu.yaml`, `gpu.kernel`**: `targets: {launch}` (was
  `{pid, launch}`), `keys: [task, proc]` (was `[gpu, task]`),
  `filter_fields` widened to what the events carry, caveats rewritten (why a
  pid is refused, what an event is, the clock, the drops, the buffer-at-a-time
  delivery). The `vendor:cupti` cost line is untouched (bench's).
- **`tests/gpu_workload.py`**: `launches`, `compute`, `memory`, `managed`,
  `streams`, `selftimed`, and a truth file (`OMN_GPU_TRUTH`) every command
  writes. The `cuLaunchKernel` argtypes fixed (see "Found").
- **`tests/gpu_kernel.rs`**: eleven scenarios (below).
- **`Makefile`, `scripts/release.sh`**: the library installed, archived,
  floor-checked and freshness-checked beside the shim.
- **`src/collect/gpu/mod.rs`** (device's file): two lines -- `mod kernel;` and
  the factory registration. Unavoidable for a new file in that directory;
  flagged here so the merge expects them.
- **`tests/family_gpu.rs` and `tests/scenario_gpu.rs`** (device's files):
  each had a test asserting `gpu.kernel` has no collector. The first now
  names `gpu.counters` instead, the second drops `gpu.kernel` from its list.
  Minimal, and needed for the suite to stay green; profiler will meet the
  same two lines when `gpu.counters` gets its collector.
- **`src/validate.rs`** (the coordinator's, review round): a refused
  target's reason is the entry's caveat that names the flag (`` `--pid` ``)
  and says "refused", when there is one, before the generic "declares no
  support". Data-driven, nine lines and a unit test; only `gpu.kernel` has
  such a caveat today, so no other message changes. Flagged so the merge
  expects it.
- **`docs/families/gpu.md`**: the section "`gpu.kernel`, through CUPTI",
  appended, with the workload table the brief asked for.

## The probe question

`omn list` answered `gpu.kernel` "no" on this machine although
`/opt/cuda/lib64/libcupti.so` is in the loader's cache. **The probe was
right**: `omn list gpu.kernel` showed `cupti  available`; the "no" was "this
machine serves `vendor:cupti`, and no collector is written for it". Nothing in
the probe changed; with the collector registered the line reads `usable
through vendor:cupti`.

## Shown versus reasoned

- **Shown** (`tests/gpu_kernel.rs`, green here, against each workload's truth
  file): kernel counts exact on all five loads (20 000 launch-bound, 50
  compute, 10 memory with 2 copies and 1 memset of exactly 8 MiB, 10 managed
  with no migration counted, 4 streams x 25 on 4 distinct stream ids, and
  overlapping in time); zero dropped with the default buffers, 50 000 launches
  included; with two 64 KiB buffers, 50 000 launches kept 41 263 and dropped
  8 737, kept + dropped = launched exactly (asserted), the run printing
  `LOSS: 8737 dropped` with the reason; a program that forks after 500
  kernels has its child exit at once and every kernel credited to the parent
  (the old library hung that child for good: the test fails after 30 s with
  it, measured); every event's `ts_err_ns` > 0; each
  self-timed kernel's CUPTI duration within [span, span + max(10 us, 1 %)] of
  its own `%globaltimer` span (measured: +1.0 to +1.3 us on 1 ms); each
  converted start and end inside the host's `CLOCK_MONOTONIC` window around
  launch and synchronise (measured: 7-10 us after the launch call, 1.2-1.9 us
  before the synchronise returns); two requests on one launch each see every
  record; a program that never calls `cuInit` says so; a `pid` target refused
  with exit 2, the refusal itself naming `CUDA_INJECTION64_PATH`.
- **Shown by unit tests**: an unsent batch adds its drop and unfinished
  counts to `lost`, not only its records; a name in an unsent batch is sent
  again under an id no other name holds; a timestamp past the last bracket
  carries the extrapolation (500 ppm before a second bracket, the measured
  slope error after).
- **Shown by hand, not asserted**: a forced flush every 50 ms returned 482 of
  500 kernels with zero timestamps, never redelivered (why the flush is never
  forced); records arrive a buffer at a time, at exit for a short run.
- **Reasoned, not measured**:
  - that the drop count is per call rather than cumulative *in general*: shown
    on this CUPTI (13.4) by kept + dropped = launched, on one release;
  - that 500 ppm bounds the slope before a second bracket: declared, not
    measured (the kernel's slew limit); it only applies to records arriving
    in the first flush period;
  - what the library costs the program: not measured (bench stream).
- **"Each kernel's duration against the self-timing kernel's own span, within
  the calibration's error"** (the brief) does not hold, and should not: the
  difference is 1.0-1.3 us, 15 times the calibration's 68 ns, because
  CUPTI's kernel includes block dispatch and drain that `%globaltimer` inside
  the kernel cannot see. The test asserts the separation instead; it is now
  a decision line below, the catalog caveat says what `ts_err_ns` does not
  cover, and the family document says why.

## What I could not do, and why

- **The clock-lock state in the facts line.** Reading it is the device
  stream's (`vendor:nvml`). Filed below.
- **An independent second reader** (`nsys`, present at `/usr/bin/nsys`): not
  done. The self-timing kernel is the independent reference the brief named;
  `nsys` would add a count and duration cross-check, at seconds per run.
- **`docs/install.md`**: its layout lists `bin/libomn_preload.so` and not the
  new library. Not my file. Filed below.
- **Live delivery for a slow program**: records come a buffer (about 1 000
  kernels) at a time, because the only faster flush loses times. A smaller
  default buffer trades callbacks for latency; 256 KiB was chosen without a
  view to judge it by. The `lanes` view will be the first to feel it.

## Privileged runs wanted

None needs root. One wants the device alone:

```console
cd /home/moha/repos/omniscient && make && nvidia-smi --query-compute-apps=pid,process_name --format=csv && cargo test --test gpu_kernel
```

on a quiet device, to record the selftimed differences and placements with
no other tenant (the numbers above were taken with the other streams of this
wave running), and once `make install-check` / `make dist-verify` to run the
release script's new lines on an extracted tree:

```console
cd /home/moha/repos/omniscient && make install-check && make dist-verify
```

`RmProfilingAdminOnly` does not matter to this stream: the activity API is not
the profiling API, and ran here without it.

## Decision lines for docs/DECISIONS.md

- **CUPTI is reached from an injection library loaded at `cuInit`**
  (wave 19): `cupti/`, a workspace member like the preload shim, named in
  `CUDA_INJECTION64_PATH`; `gpu.kernel` serves a launch and refuses a `pid`.
  Reason: CUPTI's activity API records only the process it is loaded in.
- **The injection library talks to `omn` over a sequenced-packet socket, in
  batches, with bounded sends** (wave 19). Reason: records arrive from CUPTI
  a buffer at a time on a thread that is not the program's, so a send per
  batch costs nothing per record; sequenced packets keep a forked child's
  messages whole; a timeout bounds the program's exit when `omn` stops
  reading, and what it cost is counted.
- **A forked child reports nothing** (wave 19): the library's `atexit`
  handler and every send check that the pid is the one that initialised.
  Reason: a child forked after `cuInit` cannot use CUDA and holds the
  parent's locks and CUPTI state without its threads; flushing there hung
  the child for good (measured). A child that execs loads the library
  afresh.
- **`ts_err_ns` on a GPU event is the clock conversion's error, and the
  ground truth for a duration is a separation** (wave 19). `ts_err_ns`
  bounds CUPTI's clock to `CLOCK_MONOTONIC` -- brackets, widened by the
  measured slope error past the last one -- and not CUPTI's placement of
  the device's work on its clock, nor the block dispatch and drain inside a
  duration (1.0-1.3 us here against 68 ns). A kernel's duration is held to
  span <= duration <= span + max(10 us, 1 %) of its own `%globaltimer` span,
  and its times to containment in the host's launch window. Reason: the
  upstream error is not measurable from the host, and folding a declared
  microsecond into `ts_err_ns` would make every event's error a guess.
- **A refused target's reason comes from the entry** (wave 19): the
  validator quotes a caveat naming the flag and saying "refused". Reason: the
  catalog is the data; a reason written in the validator per source is code
  per source.
- **The flush is never forced before exit** (wave 19). Reason: a forced
  flush returns kernels still running with zero timestamps and never
  redelivers them (482 of 500, measured); latency is the price.
- **A GPU timestamp's error is measured by brackets, at least 1 ns**
  (wave 19): the line through the first and last (monotonic, CUPTI,
  monotonic) brackets, error = widest half-bracket + furthest bracket from
  the line. Reason: the invariant that a converted timestamp carries a
  non-zero `ts_err_ns`, with a number that is a measurement.
- **One CUPTI socket per launch, shared by every CUPTI source of it**
  (wave 19), each source adding its `REPORT_*` bit. Reason: a program has one
  `CUDA_INJECTION64_PATH`.
- **`gpu.kernel` carries memcpy and memset too, named and marked by `op`**
  (wave 19). Reason: they are device time as much as kernels are, and a
  ranking of where the device's time went without them is short.

## Needs from another stream

- **profiler**: join the launch's socket rather than making one --
  `KernelPrepared::socket()` returns it, `Socket::subscribe` gives a reader;
  `share_launch` today downcasts to `KernelPrepared`, so either reuse that
  type with your `REPORT_*` bit or generalise the downcast in `kernel.rs`
  (tell me which and I will, or do it in the merge). Your frames go in
  `cupti/src/wire.rs` (ops from 13), your modules beside `lib.rs`;
  `InitializeInjection` arms activity on `REPORT_ACTIVITY` only, so gate
  yours on your bit.
- **device (`src/collect/gpu/{nvml,mod}.rs`)**: expect `mod kernel;` and one
  `Factory::new("gpu.kernel", "vendor:cupti", ...)` in `mod.rs`. And a way
  for `gpu.kernel`'s facts line to say whether the clocks were locked
  ("taken on unlocked clocks") -- a function I can call, or a fact the
  runner adds to every GPU timing source.
- **bench**: the `vendor:cupti` cost line, measured against
  `OMN_CUPTI_REPORT=0` (the library loaded, disarmed), on `launches 50000`
  for the worst case and `compute 50` for the common one.
- **lanes**: `stream` is the attribute for a lane, `correlation` orders
  launches; records arrive a buffer at a time (above).
- **Coordinator (`src/validate.rs`)**: review the nine-line change in
  `target_rejection_reason` (above); a `targets: {pid: "no: <reason>"}` form
  would be the stricter alternative, if you prefer it. And
  `docs/install.md`'s layout (lines 31, 432): add
  `bin/libomn_cupti.so  the CUDA injection library gpu.kernel loads`.
- **Coordinator**: the decision lines above; `docs/dev/STATE.md`'s `gpu.kernel` row.

## The suite

`make check` went red on two things this stream caused -- the two stale
"no collector" tests above, fixed -- and one it did not:
`budget::a_waiting_program_is_weighed_...` (the child's pid file not there),
which passed rerun alone. `cargo test --workspace --no-fail-fast` then ran
2368 passed, 5 failed, 11 ignored:

- `scenario_gpu::a_source_this_build_does_not_implement_...`: mine, fixed,
  passes.
- `diskstats::bytes_flushed_to_a_disk_show_up_in_its_counters`,
  `unwind::dwarf_recovers_...`, `unwind::fp_leaf_recovers_...`: in the flaky
  tally; each passed rerun alone once.
- `runner::a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed`:
  fails rerun too (75 011 against 73 971: throttles the rings do not carry);
  `docs/open-findings.md` lists it as a known gap filed for the profile
  stream, not a flake. Nothing here touches `cpu.profile`.

`tests/gpu_kernel.rs`: 10 passed, on the shared device. Clippy and fmt clean.

## Review

The adversarial review's eight findings, and what became of each.

- **`ready()` skipped for this build's own failures** (defect): fixed. The
  probe now asks the machine first -- the workload alone, with no `omn` and
  no injection; only "libcuda not loadable" or `cuInit`/`cuDeviceGet`/
  `cuCtxCreate` refused skip there -- then the workload under `omn`, where
  the only skip is the resolver's "no viable backend" (no CUPTI). Anything
  else panics, naming `make`. Checked: `OMN_CUPTI_INJECTION=/nonexistent/...`
  now fails the scenarios instead of passing ten.
- **A program that forks after `cuInit` hung its child** (defect):
  reproduced (500 kernels, fork, child `sys.exit(0)`: stuck past 40 s), and
  fixed: `at_exit` returns at once unless the pid is the one that
  initialised, and `Channel::send` drops a forked child's copy of the batch
  rather than send it under the parent's pid. The fork claim in `wire.rs`
  is rewritten. New workload `forked N` and scenario
  `a_child_forked_after_cuinit_exits_and_reports_nothing`; against the old
  library it fails after the workload's 30 s deadline, against the new one
  it passes in a third of a second. The reviewer's "also hung with
  `OMN_CUPTI_REPORT=0` under omn" is the same hang: `omn` sets
  `OMN_CUPTI_REPORT` itself in the launch environment, so the user's `0`
  never reached the program and the library was armed.
- **Uncounted losses in an unsent batch** (defect): fixed. The batch counts
  the `OP_DROPPED` and `OP_INCOMPLETE` counts it carries into `lost`, and a
  name it carried is forgotten and sent again under a fresh id (ids never
  reused). Two unit tests in `cupti/src/channel.rs`. An `OP_ERROR` lost in
  an unsent batch is still lost unsaid -- it is a message, not a record;
  the next error is sent normally.
- **The `pid` refusal without its reason** (gap): closed, in
  `src/validate.rs` (the coordinator's, flagged above): the refusal now
  quotes the entry's caveat. `--pid` prints "`gpu.kernel` cannot be measured
  per pid: A launched program only: CUPTI works inside the process that owns
  the CUDA context ...". The scenario asserts it on stderr.
- **Ground truth replaced without a decision** (gap): settled as a decision
  line above, and the catalog caveat now says what `ts_err_ns` does not
  cover. Two parts of it were fixed rather than documented: the closing
  bracket is taken *before* the forced flush, so the last buffer is
  converted between brackets instead of extrapolated past the previous
  flush's; and a record past the last bracket so far carries the
  extrapolation in its `ts_err_ns` (`ClockMap::err_at`: the slope's
  measured error times the distance, 500 ppm declared before a second
  bracket).
- **`docs/CATALOG.md` listing `pid`** (nit): fixed, `launch`.
- **The channel departing from the brief's ring** (nit): said under "What
  landed".
- **Edits in device's files** (nit): unchanged and still flagged; the
  `validate.rs` edit is added to the list.

### The suite after the review round

`make check` went red on `perf::following_tests::a_task_sampler_finds_a_thread_started_after_the_open_and_samples_it`
(the lib binary, nothing this stream touches), which passed rerun alone.
`cargo test --workspace --no-fail-fast` then ran 2303 passed, 3 failed, 11
ignored: `scenario_sched::a_launched_programs_rows_carry_the_name_it_exec_d_into`
and `scenario_sched::a_forced_switch_program_splits_the_switches_as_the_kernel_counts_them`
each passed rerun alone once; `runner::a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed`
fails again, the known gap filed for the profile stream. Clippy (`omn` and
`omn-cupti`, `-D warnings`) and fmt clean. `tests/gpu_kernel.rs`: 11 passed;
`omn-cupti`'s two unit tests pass.
