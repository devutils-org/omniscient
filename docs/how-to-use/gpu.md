# `gpu`: busy, clocks, VRAM, and what the kernels do

Lazy default: `omn gpu -- ./prog` (`gpu.engine_busy` and `gpu.mem` polled;
the busy figure is the device's).

## Two kinds of source

- **Device gauges**, read from the driver (NVML, sysfs, the i915/xe/amdgpu
  PMUs): busy, clocks, power, temperature, VRAM. Cheap to moderate, and **the
  whole device's**: a launch target sets the window, not the scope.
- **Inside the program**, through the vendor's profiling library (CUPTI for
  NVIDIA) or the preload shim: kernel times, counters, PC samples, host API
  latency. Per program, **a launched program only**, and from `med` to
  `extreme`.

## Which source for which question

| Question | Source | Mode |
|---|---|---|
| How busy is the GPU? | `gpu.engine_busy` | `poll@100ms -e timeline` |
| How much VRAM does my program hold? | `gpu.mem` | `poll@100ms` (launch or `--pid`: the process tree's) |
| At what clock, and was it held down? | `gpu.freq`, `gpu.mem_freq`, `gpu.throttle` | `poll` |
| How much energy did the device draw? | `gpu.power` | `count -e summary`; watts with `-e rate` |
| How hot? | `gpu.temp` | `poll` |
| How long does each kernel run on the device? | `gpu.kernel` | `trace -e topk` |
| How long do launches and copies take on the host? | `gpu.api` | `trace -e hist` |
| What happens inside a kernel (instructions, memory, cache)? | `gpu.counters` | `trace -e raw` |
| Where in a kernel does time go (stall reasons)? | `gpu.profile` | `trace -e stack` |
| DRM job latency (open-source drivers) | `gpu.job` | `trace -e hist` |

## Examples

```console
omn trace gpu.engine_busy,gpu.mem -m poll@100ms -e timeline,summary -- ./train.py
omn trace gpu.power -m count -e summary -- ./bench
omn trace gpu.freq,gpu.throttle -m poll@100ms -e timeline --system -d 30s
omn trace gpu.kernel -m trace -e topk:10 -- ./cuda-app
omn trace gpu.api -m trace -e hist -- ./cuda-app
omn trace gpu.profile -m trace -e stack --format folded -o out -- ./cuda-app
```

## Targets

- `gpu.kernel`, `gpu.counters`, `gpu.profile` and `gpu.api` take **a launched
  program only** (`--pid` is refused): CUPTI is loaded by the driver inside the
  process at `cuInit`, and the shim at exec.
- The gauges take a launch (as the window) or `--system`; `gpu.mem` also
  `--pid`.
- `gpu.job` needs the DRM scheduler tracepoints, which **NVIDIA's proprietary
  driver does not have**.

## Cost

- NVML calls spin in the driver: 0.8 to 7 ms of the tool's cpu per reading,
  depending on the call. A 100 ms poll of several NVML sources is a few
  percent of a core. Polling the energy counter often makes it under-count for
  every reader of the board: read it at the window's two ends (`count`).
- `gpu.kernel` costs per launch, on the launching thread: negligible for
  millisecond kernels, large for a launch-bound program. CUPTI drops records
  under a launch-bound load and the run says how many.
- `gpu.counters` **does not measure time**: kernel replay runs each launch as
  many times as the counters need, serialized. Never read a duration off a run
  that asked for it.
- `gpu.api` is a price per call (11 % on a launch every 2 us, under 3 % for
  millisecond kernels).

## Traps

- NVML's utilization is the share of a window in which at least one kernel was
  resident. It is not SM occupancy.
- A GPU at its thermal limit is a different machine: temperature is a
  confounder, not a result.
- VRAM held is not VRAM used: a CUDA context reserves an arena.
- `gpu.api` sees only calls bound through the dynamic linker; a program that
  `dlsym`s its own pointers (PyTorch) is invisible to it.
