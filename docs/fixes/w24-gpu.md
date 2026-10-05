# Wave 24, stream gpu: handoff

Branch `feat/w24-gpu`, `main` merged in at `cbb0bd4` (streams run, unwind,
triage). The driver is loaded this time (`615.71.09`, RTX 2080 Ti,
persistence mode off), so every number below is this machine's, this session.
No shared layer touched: every change is in `catalog/gpu.yaml`,
`src/collect/gpu/`, `tests/scenario_gpu.rs` and `docs/scenarios/gpu.md`.

## What landed

Defects first (`docs/scenarios/gpu.md`, defects 9 to 11):

1. **`gpu.mem` follows the launched tree** (gpu 1, defect). The process figure
   is the observed process and every descendant it has at each reading
   (`task::with_descendants`, walked once per reading), summed from the
   driver's list; a new `processes` attribute says how many hold any. The
   caveat says so.
2. **`gpu.freq`'s two lost records in the GPU bench** (defect). They were
   `late_ticks` (`losses_by_kind` of `target/bench/gpu-1791157792`: 2 late
   ticks, nothing else), caused by NVML holding a read while a CUDA context
   is created or destroyed. Not fixable in the collector; what changed is
   that it says so: every NVML event's `ts_err_ns` is now its read's span
   (was 0 for everything but the occupancy), and a run with reads held
   longer than 25 ms gets a note with the count, the worst and the reason.
3. **The NVML pollers' cost** (25 to 36 % in the backends bench). Most of it
   was one read: the end-of-run clock state, read in `stop()` inside the
   window, whose `nvmlDeviceGetCurrentClocksEventReasons` alone costs 17 to
   32 ms of cpu here. The end state is now the event reasons alone, read when
   the facts are asked, after the window (the cpu is still reported, in
   `after_window_cpu_ns`).

Features:

4. **`gpu.mem_freq`**, the memory clock as a series (gpu 7, `dmon`'s `mclk`).
5. **`gpu.throttle`**, the share of the time the clocks were held, per reason,
   from the driver's held-time counters (one `nvmlDeviceGetFieldValues` call);
   the value is the largest limiting share, each reason an attribute
   (`power_cap_ratio` is `dmon -s v`'s `pviol`) (gpu 9, throttle part).
6. **`gpu.engine_busy` carries the launched tree's own engine share**
   (`process_ratio`, `process_memory_ratio`, `processes`), `pmon`'s `sm` and
   `mem`, as attributes beside the device's series (gpu 8).

New NVML bindings in `src/collect/gpu/nvml.rs`: `nvmlDeviceGetFieldValues`,
`nvmlDeviceGetProcessUtilization`.

## Shown vs reasoned

**Shown** (this machine, this worktree, after `make`):

- `gpu.mem` under `sh -c 'python3 tests/gpu_workload.py arena 268435456 400
  700; :'`, every third point, MiB: main's binary `[0, 0, 0, 0, 0, 0, 0, 0,
  0, 0, 0]`; this branch `[0, 20, 156, 156, 412, 412, 412, 412, 412, 0]`,
  `processes` 1; unwrapped `[0, 10, 156, 156, 156, 412, ...]`. The test holds
  the 256 MiB step to the byte under the wrapper.
- NVML held reads: one call every 10 ms beside `sleep 1.3` answered in 1 to
  17 ms (three runs); beside the bench's `gpu launches 50000` with
  `BENCH_CPUS=20,21,22,23` the first call of a round took 60 to 80 ms once or
  twice per run. 30 runs of `omn trace gpu.freq -m poll@100ms -- gpu
  launches 50000` under that placement: 4 had one late tick (worst read 101
  to 112 ms), and all 4 carried the held-read note; 26 others had held reads
  (25 to 98 ms) that stayed inside the tick.
- NVML call costs on the idle board (cpu per call): temperature 0.12 ms,
  memory info 0.25 ms, a clock 2.5 ms, power draw 8.7 ms, energy 8.6 ms,
  utilization 1.7 ms, event reasons 17 to 32 ms, performance modes 8 to
  12 ms; batching the energy and the draw in one field-values call does not
  help (17.2 ms for both).
- `omn trace gpu.temp -m poll@100ms -e summary` on a 150 ms cpu burn: before
  36 to 45 ms (21.8 to 27.5 % against `low`), after 1.3 to 3.5 ms (0.75 to
  2.15 %); `after_window_cpu_ns` 17.5 ms on one run. The end-of-run line
  still appears when the reasons moved (`gpu_idle` to `none` on a CUDA
  program). `gpu.freq` on `gpu launches 50000`: 3.8 to 12.2 % (declared
  `high`).
- `gpu.mem_freq`: idle 405 MHz (`nvidia-smi --query-gpu=clocks.mem` 405);
  during `kernel 2500`, 6869 to 7069 MHz, and `nvidia-smi dmon -s c` beside it
  7069, 6869, 6869.
- `gpu.throttle --mode count`: `sleep 1.5` gives `low_utilization_ratio` 1.0
  and every limit 0; a 1.5 s kernel in a 2.09 s window 0.19; right after a
  kernel the board stays in P0 at 1950 MHz and reads 0.0 for ~1.5 s, then
  0.99 and 1.0 (why the test settles first). `dmon -s v`: `pviol` 0, `tviol`
  0 beside an idle poll whose summary was 0.
- Per-process share: `sh -c 'python3 tests/gpu_workload.py kernel 2500; :'`
  read `process_ratio` 0, 0, 0.47, 0.47, 0.73, 0.99, 0; `nvidia-smi pmon -s
  u` beside it printed 47 and 73 for the child.
- PCIe (not landed, below): during a host-to-device copy loop the workload
  measured 11.34 GB/s; `dmon -s t` 11697 to 12046 MB/s rx; NVML
  `nvmlDeviceGetPcieThroughput` 10.0 to 11.2 GB/s rx (KB as 1000 bytes).

**Reasoned, not shown**:

- A non-zero limiting share in `gpu.throttle`: provoking one needs a lowered
  power limit or a clock lock (root, the user's). The value's arithmetic is
  the same as the low-utilisation share shown.
- That the bench rows of the NVML pollers now hold their bands: the bench
  runs alone at the end of the wave (no `scripts/bench.sh` here).

## Tests

- `cargo test --lib collect::gpu`: 64 passed (new:
  `a_held_read_is_counted_and_said_and_a_prompt_one_is_not`;
  `a_driver_averaged_figure_carries_its_uncertainty` now bounds the clock's
  error by its read's span).
- `cargo test --test scenario_gpu -- --test-threads=1`: 37 passed (new:
  `a_program_under_a_wrapper_holds_the_video_memory_its_child_allocated`,
  `a_temperature_poll_costs_its_program_its_readings_not_the_clock_state`,
  `the_memory_clock_is_a_series_that_a_kernel_lifts_and_the_maximum_bounds`,
  `the_held_clocks_share_separates_an_idle_board_from_a_running_kernel`,
  `a_launched_tree_s_engine_share_rides_beside_the_device_s`).
- `cargo test --test family_gpu -- --test-threads=1`: 12 passed.
- `cargo test --test cli` (the catalog gained two entries): 28 passed.
- `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt
  --check`: clean.
- After merging main, `a_target_that_exits_without_being_reaped_ends_the_run`
  went red: stream run's `--pid` now ends when the process exits and says
  "pid N exited" before the collector says "not been reaped". The property
  (the run stops within 0.6 s of an 800 ms budget) holds; the test accepts
  either wording now.

## Not done, and why

Ranked:

1. **PCIe throughput** (gpu 9): measured (above), not landed. The catalog has
   no rate unit (`Unit` in `src/catalog.rs`, shared), and a `bytes` series of
   a rate would be summed by `rate` and `stack`. The cumulative counters
   (`NVML_FI_DEV_PCIE_COUNT_RX_BYTES`/`TX_BYTES`) are 32-bit here and wrap
   every 0.36 s at 12 GB/s. Needs a `bytes_per_second` unit (below).
2. **summary and hist per `@kernel`** (gpu 2): stream emit's; it had not
   merged when this stream finished. Nothing gpu-side is needed: `gpu.kernel`
   events carry `kernel` already, as `topk:N@kernel` reads.
3. **Launch facts as values** (gpu 6): `gpu.kernel` events already carry
   `grid`, `block`, `registers`, `shared_bytes`; as values (a summary of
   registers per kernel) they need a request to name the value attribute,
   as do `gpu.engine_busy`'s `process_ratio` and `gpu.freq`'s `memory_hz`.
   Theoretical occupancy needs the device's per-SM limits (libcuda's
   `cuDeviceGetAttribute`), not read anywhere in `omn` yet.
4. **NVTX, more driver calls, a CUPTI callback backend** (gpu 3, 4, 5):
   `shim/src/gpu.rs` (the interposed list) and `cupti/` (the injection
   library) are not this stream's files.
5. **ECC** (gpu 9): `nvmlDeviceGetTotalEccErrors` is "not supported" on this
   consumer board; nothing to show. PCIe replays are readable
   (`nvmlDeviceGetPcieReplayCounter`, 0 here) and could ride `gpu.pcie` once
   it exists.
6. **`gpu.freq`'s own per-read cost**: three clock calls, about 7 ms of cpu a
   reading on an idle board. Dropping the graphics clock (equal to the
   shader clock on every reading seen) would save a third; not done, since it
   removes an attribute without a measurement across boards.
7. **The four composed `omn_*.json` parser captures** of
   `tests/reference/captures/gpu/` (STATE item): `tests/reference/` is
   stream reference's; the driver is loaded now, so `docs/fixes/w23-gpu.md`'s
   four commands recapture them.

## Privileged runs wanted

- None needed for what landed. To show a non-zero `gpu.throttle` limit share
  (the user's call: root, a power limit, reverted after):
  `sudo nvidia-smi -pl 150 && target/debug/omn trace gpu.throttle -m count -e
  raw --format json -- python3 tests/gpu_workload.py kernel 3000; sudo
  nvidia-smi -pl 250` (the board's default limit is in `nvidia-smi
  --query-gpu=power.default_limit`), beside `nvidia-smi dmon -s v`.
- At the end of the wave, alone: `scripts/bench.sh gpu backends`, to see the
  NVML rows against their bands and whether `gpu.freq`'s late ticks still
  count as lost records.

## Decision lines

- A process target on `gpu.mem` is the process tree, walked at each reading;
  the launched pid alone missed every program under a wrapper.
- An NVML event's `ts_err_ns` is the span of its read: the value is from
  somewhere between the call and the timestamp. A read the driver held past
  25 ms is said, with the reason; a late tick it causes stays counted.
- A collector's end-of-run facts that are not about the program's window are
  read after the window, and their cpu stays in `after_window_cpu_ns`.
- A held-clocks share is the largest of the limiting reasons' shares (a lower
  bound on the union NVML does not give), clamped at 1 where the driver's two
  clocks disagree.

## Needs from another stream

- **emit / catalog (shared)**: a `bytes_per_second` (or generic rate) `Unit`,
  for `gpu.pcie` (and `nvidia-smi dmon -s t`'s pair). Reasoned: no other
  family's rate source was looked at.
- **emit (`src/request.rs`)**: a request naming its value attribute (`value:
  registers`, `value: memory_hz`, `value: process_ratio`), so attributes a
  source already carries can be summarised; that covers gpu 6 and the
  per-process share as a series without new sources.
- **coordinator, the bench** (`examples/bench/gpu.rs`, `lossy`;
  `tests/bench_gpu.rs`): a late tick is counted as a lost record there. For a
  poller whose run says the driver held its reads, that is the driver's wait,
  not records a tracer dropped; whether the GPU bench should keep late ticks
  apart from `dropped` is the bench owner's call.
- **reference**: recapture the four `tests/reference/captures/gpu/omn_*.json`
  with the commands in `docs/fixes/w23-gpu.md` (the driver is loaded now). The
  `gpu.engine_busy` events of a launch now carry three more attributes, and
  NVML events a non-zero `ts_err_ns`; no pair reads either.
- **shim / cupti owners**: `cuMemsetD8`, `cuModuleLoadData`, `cuCtxCreate` in
  the interposed list (gpu 3); NVTX ranges and a callback-API backend (gpu
  4, 5) live in `cupti/`.

## Review

- **A counted window's closing read that fails went uncounted** (gap,
  `src/collect/gpu/device.rs`). The reviewer was right: `stop` discarded the
  error of `sample(CLOSING)`, and for `gpu.power` and `gpu.throttle` in
  `count` that read is the window's only one, so a transient failure ended it
  with no event and clean losses. Fixed: `stop` now goes through
  `closing_read`, which counts a failed closing read in the collector's own
  `losses` (`failed_reads` plus the reason, absorbed by the run after `stop`),
  for every metric. A gone device counts too; the one exception is `gpu.mem`
  of a process that has exited, where the earlier readings are the series and
  the exit is how the window was meant to end. Shown by
  `a_closing_read_that_fails_is_counted` (the helper, with synthetic
  results); reasoned, not shown: a real NVML closing read failing, which the
  board here does not produce on demand.
