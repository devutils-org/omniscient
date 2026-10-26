# Stream gpu, the twenty-fifth wave

Branch `feat/w25-gpu`. Done sequentially by one agent.

## Outside the owned files, said first

- `src/catalog.rs` (the stream's two lines, and a little more): `Unit::
  BytesPerSecond`, and `Entry::stacks` (`Stacks::Device`) with
  `Entry::device_stacks()`; `gpu.profile` says `stacks: device`, and
  `collect::gpu::DEVICE_STACKS` is gone. `src/validate.rs` (the adds-up match
  and `check_stack` reading the entry), `src/encode.rs` (rendering and the
  pprof unit), `view/src/units.rs` (rendering) and `src/emit.rs`
  (`unit_named_by` knows `*bytes_per_second`) follow the new unit.

## What landed

1. **Defect: `gpu.throttle`'s shares against application clocks** on a
   driver that does not define them: left out, with a note quoting the
   driver's answer (`NvmlError::is_deprecated` is new; this board answers
   "Not Supported").
2. **`gpu.pcie`**: `nvmlDeviceGetPcieThroughput`, both directions; value
   their sum, `rx_bytes_per_second` and `tx_bytes_per_second` beside it.
3. **Launch facts as values**: `registers` and `shared_bytes` among
   `gpu.kernel`'s filter fields, so `--value registers -e summary@kernel`
   works; `op` among its keys; `process_ratio` on `gpu.engine_busy`,
   `memory_hz`/`graphics_hz` on `gpu.freq`.
4. **`gpu.api`**: `cuMemsetD8_v2`, `cuModuleLoadData`, `cuCtxCreate_v2`,
   `nvtxRangePushA`, `nvtxRangePop` appended to both lists (`family` `nvtx`).

## Shown vs reasoned

- `gpu.pcie` (shown): copy loop 13.12 GB/s by its count, `rx` median 14.13
  GB/s (headers included), `tx` 1.21 GB/s.
- `gpu.api` (shown): 1 `cuCtxCreate`, 3 `cuModuleLoadData`, 37
  `cuMemsetD8`, as the C program made them.
- `gpu.freq --value memory_hz` (shown): 405 MHz, as `nvidia-smi -q -d
  CLOCK` prints at idle.
- NVTX (reasoned): no `libnvToolsExt` on this machine.
- `--value registers` on `gpu.kernel` (reasoned): no `libcupti` on this
  machine, `gpu.kernel` is unavailable here.

## Not done

1. **`gpu.freq`'s three clock calls as one**: measured, not worth it.
   `nvmlDeviceGetCurrentClockFreqs` answers graphics and memory in one call
   at a median 4.86 ms of cpu against 5.06 ms for the three calls, and gives
   no SM clock, which is the source's value. Kept as three calls.
2. ~~**A CUPTI callback backend**~~ -- **added after the wave's first pass**:
   `gpu.api` on `vendor:cupti`, the injection library subscribing to the
   driver API domain (`cupti/src/callback.rs`, `REPORT_CALLBACK`, frames
   `OP_API` stamped in `CLOCK_MONOTONIC` inside the program) and
   `src/collect/gpu/cupti_api.rs` reading them on the shared socket; the
   other CUPTI readers ignore the new frames. Reasoned: no libcupti here;
   the callback data's layout, the frame round trip and the event are unit
   tested. Wanted: `omn trace gpu.api --backend force:vendor:cupti -e summary
   -k api -- ./cuda_program` beside the same through `preload`, the calls
   the runtime makes through `cuGetProcAddress` present only in the first.

## Tests

`--lib catalog validate encode collect::gpu`, `-p omn-preload`, `-p
omn-view`, `--test family_gpu --test scenario_gpu --test cli`, clippy, fmt.
`scenario_gpu`'s CUDA call count now expects the fixture's context creation
and module load (45 calls). `a_target_that_exits_before_the_duration_ends_
the_run_and_names_it` went red once in the full file and passed twice alone
(a window bound under load).

## Decision lines for `docs/DECISIONS.md`

- `bytes_per_second` is a level, like `watts`: no sum over readings.
- Whose stacks a source carries is the entry's `stacks:` (`device`), not a
  list in code.
- A throttle share against clocks the driver does not define is left out
  with the driver's answer, not reported as a measurement.
