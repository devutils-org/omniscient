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

## What this family needed from the shared layer

Four of these were fixed on this branch rather than requested, at the
coordinator's call; the last two stand.

### A probe for vendor libraries and hwmon names — done

`src/collect/gpu/mod.rs` exports:

```rust
pub fn probe(requirement: &str) -> Option<crate::capabilities::Probe>
```

It answers `nvml`, `cupti`, `rocprofiler` and `level-zero` by asking the dynamic
loader whether the library is there, and `hwmon:<name>` by reading the names
under `/sys/class/hwmon`. `None` means "not this family's requirement", so
`Capabilities::probe` falls through to what it already knew. The probe lives
with the code that depends on it: a family cannot be added without the means to
check what it needs.

`omn list gpu` went from nine `?` to six `yes`, one `no` and two `?`, and a
rejection now reads

```
fs:sys:amdgpu  rejected: hwmon:amdgpu: missing: no hwmon device is named
`amdgpu` (this machine has iwlwifi_1, k10temp, nvme, ucsi_source_psy_10_00081)
```

The whole listing costs 3 ms more, most of it `nvmlInit`.

### A `system` target in the runner — done

`Prepared::attach` took a pid, so every collector was handed a process whether
or not its source had one. It now takes the `Target` whole, and a launched
program is bound as the pid it turned out to be. A collector that needs a
process says which one is missing:

```
error: cannot start collecting `mem.faults`: sampling page faults machine-wide
needs per-cpu events and the privileges for them; a system target names no
process to sample
```

The `net` thread reached the same conclusion from the other end -- an interface
counter has no pid either -- and its version of the change is the one that
landed. Both were written before either branch merged, which is a reasonable
sign that a pid was the wrong thing to pass.

`gpu.mem` uses the distinction rather than refusing it: with a process it
reports that process's VRAM, without one the device's, and the `scope`
attribute says which. The two never share a series.

### A userspace filter that runs — done

A filter was parsed, validated against the source's declared fields, given a
placement in the resolved file, and never applied. `--filter "device == 99"`
kept every event. It now runs in `run_all` for any backend the resolver did not
credit with pushing it down, and what it removed is reported beside the losses:

```
  filter `bytes >= 8388608` kept 5 of 312 events, evaluated after reading
```

Events a filter removes are not losses -- they were asked to go -- but they were
collected and paid for, and a series holding five of three hundred events looks
exactly like a series of five. The run also names any field the predicate uses
that no event carried, because that is the case that empties a series and looks
like a workload that did nothing:

```
  filter `comm == 'sleep'` kept 0 of 5 events, evaluated after reading
    no event carried `comm`, so that part of the predicate was never true
```

That last line is worth reading twice: `mem.rss` declares `comm` as a filter
field and its collector never produces one. The filter was inert, so nobody
noticed.

### Fault-sampler tests that do not depend on the other tests — done

`perf::tests::a_page_fault_sample_carries_the_address_that_faulted` failed about
twice in three runs of `cargo test`, on `main`, before this family existed, and
its neighbour `sample_timestamps_share_the_tools_clock_domain` failed less often
for the same reason: it needed one sample and sometimes got none.

The cause was neither loss nor huge pages. Both tests touched a `Vec`, and the
allocator hands back memory it already holds -- already resident, and faulting
nothing. The shortfall grew with how much else the suite had allocated, which is
why adding this family's tests made it worse. Mapping the pages instead is the
fix, in both tests and in `mem.faults`'s own address test, which had it too.

Found twice independently, here and on `main`, and `main`'s version is the one
that survived the merge: it also stops asserting one fault per page touched,
which multi-size transparent huge pages make untrue on this kernel.

### Still open: the GPU in the environment capture

Two runs are comparable only if they were taken on the same device under the
same driver. Both facts are on every event this family produces, which is the
wrong place for something that cannot vary within a run: they belong in the
environment of the resolved file, beside the kernel and the governor.

Not done, for a measured reason: `nvmlInit` costs 3.3 to 4.9 ms and
`nvmlShutdown` 1.2 ms, and `Capabilities::detect` runs on every command,
including those that measure nothing. Capturing the GPU unconditionally would
put 5 ms on `omn list` for a field that matters when a `gpu` source is used. The
environment capture needs a way to ask for what the request is about before this
is worth doing.

### Still open: `Unit::Watts`

The unit enum has no watts, which is part of why `gpu.power` reports energy. Any
future source reporting an instantaneous draw -- an AMD hwmon reading, an older
NVIDIA device with no energy counter -- needs one.
