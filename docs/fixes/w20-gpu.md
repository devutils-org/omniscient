# The twentieth wave, stream gpu

Item 3 of `STATE.md` ("The GPU, what its streams left") and the defect the
bench found after the nineteenth wave (`docs/open-findings.md`, "The
nineteenth wave, coordinator, after the merge"). Branch `feat/w20-gpu`,
merged with `main` at `444900b`.

## What landed

- **`gpu.profile` no longer aborts a native CUDA program at exit**
  (`cupti/src/pcsampling.rs`). The cause, read in a debug build under `gdb`:
  CUPTI shares one `functionName` across every record of a function, the
  next drains' included (`cupti_pcsampling.h`: "should not be modified or
  freed until post processing of all the records is done"). The library
  freed the names after each drain; the next drain handed back the same
  pointer, by then reused by a Rust `Vec`, and the teardown's drain at exit
  freed it again: `free(): double free detected in tcache 2`, SIGABRT, in
  `report`'s closure dropping a row. The names are now kept in the state and
  freed once, in `finish`, after every context's last drain.
- **A target killed by the injection library is the tool's failure, said**
  (`cupti/src/{lib,wire}.rs`, `src/collect/gpu/{counters,kernel}.rs`). The
  library sends a new frame, `OP_EXITING` (32), as its exit handler starts and
  before it tears anything down. A process that sent it and never `OP_END`
  died inside `libomn_cupti.so`: `gpu.kernel`, `gpu.counters` and
  `gpu.profile` keep the lost read and add a `degraded:` line, "pid N died
  inside omn's injection library (libomn_cupti.so), in its exit handler,
  after the program's own work had finished: gpu.profile killed the program
  it measured, so the program's exit status is the tool's failure, not the
  program's". A death without the frame stays the old lost read. `omn`'s own
  exit status is not changed (see "Needs from another stream").
- **A native workload holds it** (`tests/gpu_counters.rs`,
  `a_native_program_exits_on_its_own_under_every_inside_source`):
  `examples/bench/gpu.c` built with `cc -l:libcuda.so.1`, run under
  `gpu.profile`, `gpu.counters` and `gpu.kernel`; the program's exit status
  is its own (`exit status 0`), no failed read, and its report is written.
- **Counters beside kernel times or PC sampling are refused before the
  launch, exit 2** (`src/collect/gpu/counters.rs`'s `launch_refusals`,
  called from `src/main.rs`'s `trace` and `top_live` once the requests are
  resolved; see "Review"). The rule is the one the attach already applied,
  now one function used in both places: requests that launch the same
  command line, resolved onto `vendor:cupti`, and that would ask the library
  for counters beside activity, or counters beside PC sampling, are the
  request's error. A request resolved onto any other backend is left out.
- **No unwinding warning for `gpu.profile`** (`src/collect/gpu/mod.rs`'s
  `DEVICE_STACKS`, one condition in `src/validate.rs`'s `check_stack`): its
  stacks are kernel;file:line;stall and nothing is unwound. A host profile in
  the same session is still warned.
- **The run's environment carries the GPU clock states when a `gpu` source is
  asked** (`src/capabilities.rs`: `Environment::gpu_clocks`, empty and not
  serialised otherwise; `src/resolve.rs`: the environment passes through
  `collect::gpu::with_clock_states`; `src/render.rs`: the environment block
  prints each state's `fact()`). NVML is opened once a run (a `OnceLock`),
  and a machine where it cannot be read says so in `not_captured` ("the GPU
  clock states: ...").
- **`gpu.engine_busy` accepts `raw`** (`catalog/gpu.yaml`). `gpu.mem`
  already did.
- **`docs/scenarios/gpu.md` stops naming `vendor:ncu` as a backend** (the two
  passages the item cites): each now says it records what the refusal said
  then, and that `gpu.kernel`, `gpu.api` and `gpu.counters` have collectors.

Commits: `0f26fa6`, `4520db2`, `f79967b`, `4bce392`, `f255af7`, `320c4c3`,
`2dba7bf`, and this handoff.

## Shown versus reasoned

Shown, on this machine (RTX 2080 Ti, driver 615.71.09, other agents on the
machine, not on the device):

- The abort, reproduced 1 of 1 before the fix
  (`omn trace gpu.profile -m trace -e stack --backend force:vendor:cupti --
  gpu compute 50 200000`: "stopped because the launched program exited with
  signal 6"), its stack under `gdb` (main thread in `exit` ->
  `omn_cupti::at_exit` -> `pcsampling::finish` -> `tear_down_one` -> `drain`
  -> `report` -> drop of a `Vec<u8>` -> `abort`), and 3 of 3 runs clean after
  it: exit status 0, no failed read, the report written.
- The new test **fails on the code before the fix** and passes after it (run
  both ways, twice after). With a 50 ms warm-up instead of the default 500 ms
  it passed on the old code too: one drain is not enough for a name to come
  back, so the test keeps the default and says why.
- The failure line, shown by putting the old `pcsampling.rs` back for one
  run: the `degraded:` line above appears beside the lost read, and the loss
  note says the program died inside the injection library.
- The refusal: the two-request session exits 2 with
  "[counters] gpu.counters cannot be asked with gpu.kernel in one run: ...",
  and the program is not started (no stdout warning about the launched
  program's output, which the run printed before).
- The environment: `--dump-request` of a `gpu` source prints
  "vendor:nvml: device 0 (NVIDIA GeForce RTX 2080 Ti): shader clock not shown
  locked ..."; `--dump-resolved` of `mem.rss` has no `gpu_clocks`.
- `gpu.engine_busy -m poll -e raw --system` prints its events.
- **`kernel.rs`'s read path, profiled** (release build with symbols,
  `perf record -F 2999 -g` over `gpu.kernel -e summary` on
  `gpu launches 200000`): `omn` spent 281 to 368 ms of cpu over 200 048
  records, 1.4 to 1.8 us a record. Of `omn`'s samples, about a third is
  libc (`malloc`, `free`), a third the kernel (page faults: the held events
  growing), a third `omn`. `KernelCollector::drain`, decoding and stamping
  included, is about 23 % of `omn`'s cpu; cloning events' `Vec<Attr>` after
  the collector (the runner and `emit`) about 18 %, `emit::aggregate` 8 %,
  dropping events 12 %. Each kernel event allocates about twenty strings: a
  `String` per attribute name (12), the text values, `kind`, `collector_id`,
  the keys. So the cost per record is the event model and its copies, not
  CUPTI's records or their decoding.

Reasoned, not shown:

- **No lighter activity kind** is a fallback: `CUPTI_ACTIVITY_KIND_KERNEL`,
  the only other kernel record, serialises the program's kernels, which
  changes what is measured; the per-launch cost inside the target (about
  1.3 us) is CUPTI's own, and what `omn` spends is the event model above, which
  a different kind would not change.
- The OP_EXITING frame is sent with a blocking send bounded by the channel's
  send timeout; a library that cannot send it falls back to the old lost read,
  which is the conservative side.

## What I could not do, and why

- **`omn` exiting non-zero when the tool killed its target**: there is no
  channel from a collector to the run's exit status (`Collector` has
  `notes`, `facts`, `losses`); that is `src/collect/mod.rs` and
  `src/main.rs`, the budget stream's. Filed below.
- **`gpu.temp` over `low` on the compute-bound regime**: left `low`. The two
  tables disagree (+0.88 % then +1.11 %, with a 0.56 % gate), the record
  still holds the first table, and `tests/bench_gpu.rs`'s record check fails
  if the catalog's band and the record's disagree, so changing the band
  without a new table pasted in would break the record. The next GPU bench
  (which the double-free fix unblocks) decides it: over 1 % again, `med`.
- **The GPU bench's two hours**: the schedule is `examples/bench/gpu.rs`
  and `tests/bench_gpu.rs`, not this stream's files. Filed below.
- **`target_rejection_reason` against a `targets: {pid: "no: <reason>"}`
  form**: reviewed, not changed. The structured form is the better one: the
  reason sits on the axis it explains and is data the loader can hold to
  being present, where today's form finds it by matching a caveat that
  contains both "`--pid`" and "refused", and silently falls back to "the
  catalog declares no `pid` support" when a caveat is reworded. Changing it
  is the catalog loader's `TargetSupport` parsing (`src/catalog.rs`, the
  shared stream's) and every entry that refuses a target with a reason.
  Filed below and as a decision line.
- **`docs/bench.md` lines 617-618 and `CATALOG.md`'s NVML cost cells**: not
  this stream's files. Filed below.
- The AMD and Intel kernel paths wait for a card in the machine.

## Privileged runs wanted

None needs root. Alone on the machine and the device:

```console
# the GPU bench, now that gpu.profile can be measured (about two hours until
# the launch-bound schedule is shortened)
scripts/bench.sh gpu
# the inside sources on a device nobody else uses
cargo test --test gpu_counters --test gpu_kernel -- --test-threads=1
```

## Decision lines for DECISIONS.md

- **A cost per regime in the catalog's schema** (proposed, with numbers).
  The bench's two tables give, for one backend, bands two orders of
  magnitude apart by regime: `gpu.kernel` via `vendor:cupti` +64.5 % on
  `launch`, +1.81 % on `compute`, +0.15 % on `memory`; `gpu.api` via
  `preload` +10.5 / +2.75 / +0.01 %; `gpu.counters` +282 934 / +351 /
  +1 841 %. The proposal: `cost` stays the worst regime's band, the one the
  bench gates and the resolver ranks by; a backend may add
  `cost_by_regime: {launch: extreme, compute: med, memory: low}`, checked by
  the bench row by row, read by the budget's first check and printed by the
  run's cost line beside the measured figure. **What leaving it open costs**:
  the budget's check, once it stops a run on a declared band, stops
  `gpu.kernel` on a compute-bound program that it slows by under 2 %, and the
  cost line says `extreme` where the measurement says `med`.
- **A refused target's reason in the catalog** (proposed): `targets:
  {pid: "no: <reason>"}` replaces finding the reason in a caveat; the loader
  refuses a `no` without a reason. What leaving it open costs: a reworded
  caveat silently degrades a refusal to "the catalog declares no support".
- **A target the tool killed** (settled here for the GPU, proposed for every
  injector): a library loaded into the target says when it starts its exit
  path, and a death after that is the tool's failure, said as such, never
  only a lost read.

## Needs from another stream

- **budget** (`src/collect/mod.rs`, `src/main.rs`): a channel from a
  collector to the run's outcome -- a `fn failure(&self) -> Option<String>`
  on `Collector`, say -- so that "the tool killed the program it measured"
  makes `omn` exit non-zero (not 0 as now). The GPU collectors would answer
  it from the same condition that writes today's `degraded:` line
  (`counters::died_in_exit`).
- **shared** (`src/catalog.rs`): the `targets: {pid: "no: <reason>"}` form
  above, if the decision is taken; `validate.rs`'s
  `target_rejection_reason` then reads it rather than searching caveats.
- **docs** (`docs/bench.md` lines 617-618, `CATALOG.md`): stop naming
  `vendor:ncu` as a backend; `CATALOG.md`'s `gpu.engine_busy` row gains
  `raw`, and its NVML cost cells follow `catalog/gpu.yaml`.
- **bench schedule** (`examples/bench/gpu.rs`, `tests/bench_gpu.rs`, nobody's
  this wave): fewer launches on the `launch` regime for a replaying arm
  (`gpu.counters`: 108 of 117 minutes), e.g. a launch count per arm, so the
  GPU step fits in about fifteen minutes again.
- **coordinator**: `src/validate.rs` (`check_stack`), `src/main.rs`
  (`launch_rejections` and its two calls, after the review),
  `src/resolve.rs` (one call) and `src/capabilities.rs` (one field) were
  touched outside this stream's row, because the items assigned here could
  not be done otherwise; each is a few lines and calls into
  `src/collect/gpu/`.

## Review

The fix round, after the merge (`main` merged in first, a fast-forward).

- **`launch_refusals` decided from source names, before resolution**
  (fixed). It took an unpinned request as CUPTI's, so a pair that would
  resolve onto another vendor's library was refused with CUPTI's
  kernel-replay reason. It now takes each request with the backend the
  resolver chose and counts only those resolved onto `vendor:cupti`; the
  call moved out of `validate_session` (which reverts that edit) into
  `src/main.rs`, after resolution and before any dump or launch, in `trace`
  and in `top_live`, beside the budget's refusal, as a `Rejection` the same
  renderer prints. Consequences: `omn validate` of such a session now says
  it is valid (it never resolves, and the rule is about what serves the
  requests), and on a machine where CUPTI does not resolve the resolver
  answers first, so `gpu_counters`'s refusal test now skips there instead of
  expecting exit 2 everywhere. The unit test holds the new half: the same
  pair resolved onto `vendor:rocprof`, or one of the two onto it, is not
  refused.
- **Edits outside the stream's files** (agreed, nothing to fix here): the
  list is under "Needs from another stream", coordinator, now with
  `src/main.rs` in place of `validate_session`. They are for the
  coordinator to land as shared-layer changes.
- **A target the tool killed still exits 0** (agreed, not fixed here). The
  `degraded:` line says it, but a script reading `omn`'s exit status cannot
  tell. Making it a failure needs a channel from a collector to the run's
  outcome: a `Collector` method (`src/collect/mod.rs`), carried on
  `Collection` through the three places the runner assembles one, and read
  by `src/main.rs`'s `measure` to exit 1 (a failure of this run, as an
  attach failure is). That is the collector layer and the CLI, not this
  stream's, and a new outcome channel is a decision for every family, not a
  GPU fix; it is filed under "Needs from another stream" (budget) with the
  GPU side ready to answer it from `counters::died_in_exit`'s condition.
  The read the target took with it stays counted in `failed_reads`: the
  last drain did not arrive, so that loss is real and is said as well.

## The tests

- `cargo test --lib collect::gpu`: 63 passed. `cargo test -p omn-cupti`:
  10 passed.
- `family_gpu` 12, `gpu_kernel` 11, `gpu_counters` 9 passed;
  `scenario_gpu` 31 of 32, the one failure
  (`a_target_that_exits_before_the_duration_ends_the_run_and_names_it`, a
  `gpu.mem --pid` window under load) passed when rerun alone.
- Beside them, for the shared files touched: `--lib validate` 23,
  `--lib render` 14, `--lib capabilities` 34, `--lib resolve` 28,
  `--test cli` 26, `--test cross_machine` 6, all passed.
- `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo fmt --check`: clean.
- Fix round: `cargo test --lib collect::gpu` 63, `--lib validate` 23,
  `--bin omn` 6, `--test family_gpu` 12, `--test gpu_counters
  counters_beside` 1 (run, not skipped: the device and CUPTI are here), all
  passed; `omn -f` of the counters-and-kernel session shown exiting 2 with
  the refusal. Clippy and fmt clean.
