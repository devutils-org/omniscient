# Family `gpu`

What is written, what it was checked against, and what the numbers do not mean.
The vocabulary is in `CATALOG.md` section 8 and in `catalog/gpu.yaml`; this file
is the measurement log.

## The asymmetry is the design

AMD and Intel expose a DRM scheduler, kernel PMUs and hwmon: their GPU numbers
come from the kernel like every other family's. NVIDIA exposes none of that.
There is no `gpu_scheduler` tracepoint on the proprietary driver, no
`/sys/bus/event_source/devices/nvidia`, and no hwmon node for the board. Below
the driver there is nothing to read.

So the NVIDIA path goes through `libnvidia-ml.so`, opened with `dlopen` at
runtime and called directly. That is the same choice the rest of the project
makes when it calls `perf_event_open` rather than running `perf`: the library
is the interface, and a tool that instead ran `nvidia-smi` and parsed its
output would depend on a text format nobody promised to keep. `nvidia-smi`
appears in this family exactly once, in a test, as an independent second reader
to check our numbers against.

Opening at runtime rather than linking keeps the build free of any vendor SDK,
and turns "is the driver installed" into an ordinary probe answer.

## What is implemented

| Source | Backend | Value | Scope |
|---|---|---|---|
| `gpu.freq` | `vendor:nvml` | shader clock, Hz | device |
| `gpu.power` | `vendor:nvml` | energy since the run started, J | device |
| `gpu.temp` | `vendor:nvml` | °C | device |
| `gpu.engine_busy` | `vendor:nvml` | ratio | device |
| `gpu.mem` | `vendor:nvml` | bytes held by the target process | process |

Every event carries the device index, its UUID, the device name, the driver
version and a `scope` attribute, so a result file can be read without the
command line that produced it.

## Measured on a GeForce RTX 2080 Ti, driver 615.71.09

### Per-process VRAM is exact

A CUDA context was created and exactly 256 MiB allocated through the driver
API. The collector reported the difference to the byte: 268435456. That is the
ground truth behind `gpu.mem`, and it runs in `cargo test` on any machine with
the driver (`a_known_allocation_is_reported_as_exactly_that_many_bytes`).

The same measurement shows what `gpu.mem` cannot see: an empty CUDA context
already holds **163.6 MB** of VRAM on this driver, before a single allocation.
VRAM held is an arena, not usage.

### Integrating sampled power is not energy, and polling faster makes it worse

`gpu.power` reports the device's own energy counter rather than an integral of
the sampled draw. The two were compared over the same windows, round-robin, on
an idle-to-lightly-loaded device:

| Sampling of the draw | counter ÷ integral |
|---|---|
| 1 Hz | 0.897, 0.945, 0.974 |
| 20 Hz | 0.746, 0.637, 0.748 |

Integrating the sampled draw overstates the energy by about 5 % at 1 Hz and by
about a third at 20 Hz. More samples make the integral *worse*, which is the
opposite of what an integral is supposed to do: `nvmlDeviceGetPowerUsage`
returns a value the driver has filtered, and resampling it at 20 Hz reads the
same filtered value repeatedly rather than adding information.

This is why the source is declared in joules and reads the counter, and why a
device without an energy counter is refused rather than served an integral
under the name of energy.

### The device totals agree with the vendor's tool

VRAM total matches `nvidia-smi` exactly, temperature within the tolerance of
two readings a moment apart. Both checks are in `tests/family_gpu.rs` and skip
with a reason where there is no GPU or no `nvidia-smi`.

## Traps

- **NVML's utilization is not occupancy.** `gpu.engine_busy` reports the
  fraction of a driver-chosen window during which at least one kernel was
  resident. A kernel using one of the 68 SMs and a kernel using all of them
  both read 100 %. It answers "was the GPU doing something", never "was the GPU
  busy enough".
- **That window is not reported, so its events carry `ts_err_ns` of one
  second**, the documented upper bound of the driver's sampling period. The
  clock, temperature and memory readings are taken at the call and carry zero.
- **A launch target sets the window, not the scope.** `omn gpu.temp -- ./prog`
  measures the device while `prog` ran, together with the compositor, the
  browser and everything else with a context open. Only `gpu.mem` attributes
  anything to a process, because the driver keeps a per-process figure. The
  `scope` attribute on every event says which of the two it is.
- **NVML reports host pids.** Inside a pid namespace the driver's process list
  is in the host's numbering, so a `--pid` taken from inside a container will
  not match. Nothing here corrects for it yet.
- **`gpu.job`, `gpu.kernel` and `gpu.counters` are not written.** The first
  needs the DRM scheduler tracepoints, which this driver does not have and the
  eBPF layer does not exist for; the other two need vendor profiling SDKs whose
  cost the catalog marks `med` and `extreme`. They resolve and say plainly that
  no collector exists, which is the honest answer.
- **The sysfs backends for AMD and Intel are declared, not written.** A reader
  of a sysfs layout nobody can exercise is a guess with a file path in it. Their
  catalog requirements name the bound driver -- `/sys/bus/pci/drivers/amdgpu`
  -- rather than `/sys/class/drm`, which exists on every machine with a
  display, NVIDIA ones included, and would have made the resolver pick a
  backend with nothing to read.

## Needs from the shared layer

### A probe for vendor libraries and hwmon names

`src/collect/gpu/mod.rs` exports the probe this family needs:

```rust
pub fn probe(requirement: &str) -> Option<crate::capabilities::Probe>
```

It answers `nvml`, `cupti`, `rocprofiler`, `level-zero` by asking the dynamic
loader whether the library is there, and `hwmon:<name>` by reading the names
under `/sys/class/hwmon`. `None` means "not this family's requirement". Wiring
it into `Capabilities::probe`, before the final `match`, is one line:

```rust
if let Some(probe) = crate::collect::gpu::probe(requirement) {
    return probe;
}
```

Until that lands, every `gpu` source reads `?` in `omn list` and every run
reports `unverified requirement: nvml`, which is the honest reading of "no
probe covers this" -- but it is wrong about this machine, where the answer is
known and checkable.

### A `system` target in the runner

`run_all` maps a target to a pid and refuses anything that is not a pid, a tid
or a launch. Every device-scoped source in this family -- and `power.energy`,
and `mem.bandwidth` -- is declared `system` in the catalog and can only be run
here by naming a program whose window to use. `omn gpu.temp --system -d 30s` is
the natural command and it does not exist.

### A userspace filter that runs

A filter is parsed, validated against the source's declared fields, and given a
placement in the resolved file. Nothing then applies it to the events:

```console
$ omn gpu.temp --mode poll@200ms -e summary --filter "device == 99" -- sleep 0.4
  summary
    <unkeyed>      n=4     min 39 celsius  mean 39 celsius  max 39 celsius
```

Four events matched a predicate that excludes them all. `crate::filter::matches`
exists and is used by the join engine; it is the collection path that never
calls it. This is a silent wrong number, which is the one thing the project
says it will not do, and it is not confined to this family.

### The GPU in the environment capture

Two runs are comparable only if they were taken on the same device under the
same driver. Those two facts are on every event this family produces, which is
the wrong place for something that does not vary within a run: they belong in
the environment of the resolved file, beside the kernel and the governor.
`Environment::not_captured` currently lists "vendor SDK versions".

### The checks do not pass on `main`

Besides the flaky test below, `cargo fmt --check` reports `src/collect/mem/rss.rs`
and `src/collect/mod.rs`, and `cargo clippy` reports an unused import in
`src/collect/mod.rs`. Both arrived with the per-family registry commit and
neither belongs to this family. The gate in `CLAUDE.md` is red before any family
thread touches anything.

### A page-fault sampler test that does not depend on the other tests

`perf::tests::a_page_fault_sample_carries_the_address_that_faulted` demands all
64 of its faults and fails when it gets 63. It already fails on `main` about
twice in three runs of `cargo test`, before this family existed; the sampler is
attached to the process and its period is consumed by faults from every other
test running at the same time, so the number it gets depends on what else the
suite is doing. This family's tests make it worse -- creating a CUDA context
faults hundreds of megabytes -- but the fragility is in the test, not in what
loads the machine beside it. A sampler that legitimately sees fewer samples
under load should be checked for the property it promises, not for a count.

### `Unit::Watts`, if a sampled-power source is ever wanted

The unit enum has no watts, which is part of why `gpu.power` reports energy.
Any future source that reports an instantaneous draw -- an AMD hwmon reading,
an older NVIDIA device with no energy counter -- needs one.
