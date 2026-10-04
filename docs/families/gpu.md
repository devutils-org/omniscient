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
| `gpu.api` | `preload` | time inside one host API call, ns | the calling thread |

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


---

# `gpu.job`, written here and exercised elsewhere

Added in the sixth wave, and it is the exception in it. Everything else this
project has landed was checked against something on the machine it was written
on. This was not, and cannot be, and saying so is the point of this section:
**a fix you cannot exercise is a claim**, and what follows separates the parts
of it that were checked from the parts that were not.

## Why it cannot be exercised here

Two independent reasons, and either alone would be enough.

The machine runs the NVIDIA proprietary driver, which has no DRM scheduler.
`drm_run_job` and `drm_sched_process_job` are defined by the `gpu_scheduler`
module that AMD's `amdgpu` and Intel's `i915` and `xe` drivers use, and below
the NVIDIA driver there is nothing of the kind to trace. The catalog entry has
said so as a caveat since it was written.

And they are module tracepoints, so they carry no `btf_trace_` type in
`vmlinux` BTF: `BPF_RAW_TRACEPOINT_OPEN` cannot find them by name. This is the
only collector in the build on the classic tracepoint path, which needs a
numeric id and a field layout out of `/sys/kernel/tracing` — and on this
machine tracefs is mounted and not readable by the uid the tool runs as. So
even the field layout the collector reads at startup cannot be looked at here.

The answer the tool gives is unchanged by the collector existing, and that is
itself checked:

```console
$ omn gpu.job --system -m trace -e hist -d 1s ; echo $?
error: `gpu.job` has no viable backend on this machine:
  bpf:tp: tracepoint:gpu_scheduler:drm_run_job: missing: tracefs is mounted but
  not readable by uid 1000, ...
  hint: the request is valid; it is this machine that cannot serve it.
1
```

Exit 1, the machine — not exit 3, the build. Before this wave both were true
and a reader could not tell which; now only one is, and
`tests/scenario_gpu.rs::the_collector_exists_and_this_machine_is_still_the_thing_that_cannot_serve_it`
holds the distinction.

## What was checked, and how

Everything that does not need the hardware, which is more than it sounds.

**The record layout, against the text real kernels write.** A classic
tracepoint's record is a structure the kernel lays out itself, and its fields
move: the `format` files of `drm_run_job` and `drm_sched_process_job` as Linux
6.12 writes them are in `src/bpf/sched_drm.rs` verbatim, and the parser is
checked against them field by field — offsets, widths, and which fields are
`__data_loc` rather than values.

**The refusal for a kernel this program would read wrong.** Older kernels
identify a job by its `struct dma_fence *`, and the DRM scheduler only began
publishing the fence's context and sequence number in Linux 6.11. This build
refuses the pointer rather than falling back to it, and says why: a fence
pointer is recycled as soon as the fence is freed, so a completion would be
attributed to whichever later job landed on the same address. It is the same
argument the catalog makes for scoping block requests by `(ptr, open_ts)`, and
the same one `docs/scenarios/io.md` will make when `io.latency` lands.

**That each program reads its own record's offsets.** The two records are laid
out differently — `fence_context` is at 40 in one and at 8 in the other — so a
program using the other's offsets would key completions by whatever happens to
be at 40. There is a test that crosses them.

**The event's shape.** The catalog gives this source a `gpu:fence_ctx+seqno`
join key, which is why finished jobs are one row each rather than a bucket: an
aggregate has no sequence number to join on. A GPU retires thousands of command
buffers a second, not the hundreds of thousands a lock tracepoint sees, so one
row each is affordable here where for `sched.lock` it would not be.

## What was not checked, and is therefore a claim

- **That the programs load.** The verifier has never seen them. They are
  `BPF_PROG_TYPE_TRACEPOINT`, which reads its context directly rather than
  through `bpf_probe_read_kernel`, and that is a different validity rule from
  every other program in this build.
- **That the attach works.** `attach::tracepoint` is exercised by nothing else
  in the build either.
- **That the numbers mean what they say.** Submission to fence, IRQ included,
  is what the tracepoints describe; nobody here has compared one against a
  workload that submitted a known number of command buffers.

A machine with `amdgpu`, `i915` or `xe` and a readable tracefs can run
`examples/gpu-job-latency.yml` and find out. `tests/scenario_gpu.rs` prints
what it found about this machine — the tracefs root, whether the
`gpu_scheduler` events are readable, and what is under `/sys/class/drm` — so
that the next person to read the output knows which machine they are on.

## Two things the catalog promises that this build bends

**`ring` is not the tracepoint's ring name.** The entry offers `ring` as a
filter field. The tracepoint's own name field is a `__string`, which arrives as
a `__data_loc` — a packed offset and length — and reading the string it points
at needs a variable-offset read of the record that this eBPF layer does not
have. So the events carry the name of the *kernel thread that ran the job*
instead, which on every driver with a DRM scheduler is named after the ring it
serves. `ring` and `comm` are the same string, every run says where it came
from, and the missing mechanism is written up under "Needs from another stream"
in `docs/open-findings.md`.

**A `pid` target is refused.** The entry declares `pid: bpf_only`.
`drm_run_job` fires in the DRM scheduler's own kernel thread, so the process
that submitted the work is not in the tracepoint's context and there is nothing
to compare a pid against. Attributing a job to a process needs the submission
path — `drm_sched_job_arm`, or the driver's own ioctl — which this build does
not attach to. The refusal says that, and is marked as this build's gap rather
than as a request that cannot mean anything.

# `gpu.api`, through the preload shim

The host's side of GPU submission: how long the calling thread spent inside
each call into the CUDA driver API or Vulkan. The ninth wave wrote it on the
`LD_PRELOAD` shim the heap and mutex sources share; the shim's half is
`shim/src/gpu.rs` and the reader's `src/collect/gpu/api.rs`.

The brief expected this to be unreachable on a headless machine with the
proprietary NVIDIA driver, and allowed for a collector exercised elsewhere, the
way `gpu.job` is. **It is reachable, both halves of it.** The CUDA driver
launches kernels headless, and Vulkan needs no display either: an instance,
the first physical device, a compute queue and a fence are enough to submit
and wait. Both are measured here and neither scenario skips on this machine.

## What an event is

One per call into an interposed entry point, stamped when the call was
entered, carrying `latency_ns` (the value), `api` (the entry point without the
`_v2` ABI suffix `cuda.h` binds, so `api == 'cuMemcpyHtoD'` means what it
says), `family` (`cuda` or `vulkan`), `result` (the return code, sign-extended:
Vulkan's positive codes are statuses, not failures), and `tid`, `pid`, `comm`.

Interposed: `cuLaunchKernel`, `cuMemcpyHtoD`, `cuMemcpyDtoH`, their `Async`
forms, `cuCtxSynchronize`, `cuStreamSynchronize`, `cuEventSynchronize`,
`cuMemAlloc`, `cuMemFree`; `vkQueueSubmit`, `vkQueueSubmit2`,
`vkQueueWaitIdle`, `vkDeviceWaitIdle`, `vkWaitForFences`, `vkQueuePresentKHR`.
Not OpenGL: GL's work is submitted inside the driver at points no entry point
names, and `glFinish` alone would be a source of one call.

The catalog entry changed in two ways to stop promising what no event carries.
`keys` was `[gpu, task, req]` and is `[task]`: the device a call went to is its
context's, and asking the driver inside every call would cost more than the
call. `targets` was `{pid, launch}` and is `{launch}`: a preloaded library is
given at exec, and the only backend is the preload.

## Which programs it can see -- the thing to know first

`LD_PRELOAD` wins the dynamic linker's **global** lookup. So the shim sees a
call bound through the program's own relocations -- a C program linked with
`-lcuda` or `-lvulkan` -- and nothing else. It does not see a call through a
pointer the program fetched for itself:

| How the program reaches the API | Seen |
|---|---|
| linked with `-lcuda` / `-lvulkan`, calling the exported symbols | **yes** |
| `dlopen` + `dlsym(handle, ...)` -- Python's `ctypes`, most language bindings | no: a lookup on a handle searches that library and its dependencies, never the preload |
| the CUDA **runtime** (`cudart`, so PyTorch, CuPy, anything `nvcc` builds by default) | no: it fetches every driver entry point with `cuGetProcAddress` |
| `vkGetDeviceProcAddr` -- most Vulkan engines, for speed | no |

That second half is most real GPU programs. Measured, with the workload every
other scenario in this family uses (`tests/gpu_workload.py`, ctypes):

```
gpu.api  0 events over 586.76ms, stopped because the launched program exited
  degraded: no GPU API call was reported for pid ...: the shim sees only calls
  bound through the dynamic linker, so a program that reaches the driver through
  `dlopen` and `dlsym` (Python's ctypes), `cuGetProcAddress` (the CUDA runtime
  does, for every driver call) or `vkGetDeviceProcAddr` reports nothing here
  whatever it submitted
```

while `gpu.engine_busy` sees its kernel. That sentence is the scenario:
`examples/gpu-api-unreachable.yml` asserts zero events *and* the note, because
a silent zero here reads as an idle GPU. (It was silent the first time it ran:
see "Found here, belongs elsewhere" in `docs/families/sched.md` -- a loss reason
beside clean counters is never rendered.)

What would reach the rest is interposing the pointer-fetching calls themselves
-- `cuGetProcAddress`, `vkGetDeviceProcAddr`, and ultimately `dlsym` -- and
handing back wrappers. It is possible and it is how vendor tracers work; it is
also the point where a shim stops being a few forwarding functions and starts
being a second loader, and it was not attempted here.

## Ground truth

The fixture is a C program in `tests/scenario_gpu.rs`, built twice by the test:
`gpu-cuda` against `libcuda.so.1` -- with the few prototypes it needs declared
in the fixture, so the driver is enough and the toolkit is not needed -- and
`gpu-vulkan` against the loader. Its command line chooses how many calls it
makes, and it times every call itself with `CLOCK_MONOTONIC`. The shim is
inside the call the program timed, so for each entry point the tool's total is
nested in the program's.

`examples/gpu-api-cuda.yml`: a word of device memory, 20 launches of a spin
kernel each followed by `cuCtxSynchronize`, a copy back and a free -- 43 calls.

```
gpu.api  43 events, no loss
cuLaunchKernel 3951 ns on average, cuCtxSynchronize 1887477 ns
```

Asserted: every entry point's count equals the program's, exactly, and
**nothing else was reported** -- 43 events for 43 calls, which is also the
check that the driver does not call its own exported symbols internally, where
the shim would have counted them twice. Every total is at most the program's
own. And the separation this source exists for: a launch is queued and returns
in microseconds, and the kernel's two milliseconds are paid by the synchronize
that waits for it; the scenario requires a factor of ten and the run shows
nearly five hundred.

`examples/gpu-api-vulkan.yml`: 100 empty submissions, a fence wait after each,
and a queue wait at the end -- 201 calls, 201 events, each count exact and
every event `family: vulkan`. The fixture prints the device it found
(`NVIDIA GeForce RTX 2080 Ti`); on a machine with no Vulkan device or no
compute queue it prints `unavailable=` with the call that refused, and the
scenario skips with that.

## What is not measured, and why

- **Per entry point, the summary cannot split.** `api` is an attribute and not
  a correlation key, so `summary`, `hist` and `topk` group by thread. A
  breakdown by call is `--filter "api == 'cuCtxSynchronize'"`, one call at a
  time, or `raw`. This is the same gap as "`topk` ranks a series, not an
  attribute" in `STATE.md`, and needs surface syntax nobody owns.
- **Which device.** No `gpu` key, for the reason above.
- **A program with no driver behind the shim** gets the API's own "not
  initialized" code (`CUDA_ERROR_NOT_INITIALIZED`,
  `VK_ERROR_INITIALIZATION_FAILED`) from the wrapper rather than a jump to
  address zero. That changes one behaviour: a program that probes for
  `cuLaunchKernel` in the global scope with `dlsym(RTLD_DEFAULT, ...)` finds
  it, where without the shim it would not. `cuInit` is not interposed, so a
  program that probes for that -- the usual one -- is unaffected.
- **Cost.** One `write(2)` per call, the same mechanism as `sched.mutex`,
  whose section measures it at about 415 ns per record in a release build.
  Against a `cuLaunchKernel` of four microseconds that is a tenth of what is
  being measured, and it is inside the interval the program itself would see.

## Index rows for the coordinator

For `examples/README.md` and `GUIDE.md`, `gpu` table:

```
| `gpu-api-cuda.yml` | How long the host spends inside the CUDA driver API, call by call. |
| `gpu-api-vulkan.yml` | How long the host spends submitting to a Vulkan queue and waiting for it. |
| `gpu-api-unreachable.yml` | A program that drives the GPU where the shim cannot see it, and says so. |
```

# `gpu.kernel`, through CUPTI

The device's own account of what ran: one event per kernel, memcpy and memset,
with its time on the device. The nineteenth wave wrote it on CUPTI's activity
API, reached from an **injection library** (`cupti/`, built as
`libomn_cupti.so`) that the CUDA driver loads into the launched program at
`cuInit` because `omn` names it in `CUDA_INJECTION64_PATH`. The reader is
`src/collect/gpu/kernel.rs`; the format both sides compile is one file,
`cupti/src/wire.rs`, included by path on the `omn` side so the two cannot
disagree on a number.

## What an event is

Stamped at the activity's start on the device, converted to `CLOCK_MONOTONIC`,
with `ts_err_ns` the calibration's measured error, widened by the slope's
measured uncertainty for a record later than the last bracket so far (or by a
declared 500 ppm before a second bracket exists). That is the error of the
conversion from CUPTI's clock, and of nothing upstream of it: see "Ground
truth" for what the device's own placement adds. Attributes: `duration_ns`
(the value: end minus start on the device), `op` (`kernel`, `memcpy`,
`memset`), `kernel` (the demangled kernel name, or `[memcpy HtoD]`,
`[memcpy DtoH]`, ..., `[memset]`), `device`, `stream`, `context`,
`correlation`; for a kernel `grid` and `block` (`68x1x1`), `registers` and
`shared_bytes`; for a copy `bytes` and `copy` (the direction); for a memset
`bytes` and `value`; and `pid`. Keys are `task` (the process's main thread)
and `proc`: an activity record names its device, context and stream, never the
thread that launched it, and the `gpu` key is a fence, which a kernel record
does not carry. Rank with `topk:10@kernel`; count only kernels with
`--filter "op == 'kernel'"`.

A `pid` target is refused: CUPTI records the process it is loaded in, and the
driver loads it at `cuInit`, which a running program did long ago. The
catalog's `targets` is `{launch}` and its first caveat says why; the
validator quotes a caveat that names the refused flag (`` `--pid` ``) and says
"refused" as the refusal's reason, so `--pid` prints it.

## The channel

One `AF_UNIX` `SOCK_SEQPACKET` socket per launch, made by `omn` and named in
`OMN_CUPTI_FD`. Sequenced packets keep message boundaries and never interleave
two senders, so every process that loads the library -- a child that execs a
CUDA program loads it afresh -- sends whole messages, each saying whose they
are. A child *forked* after `cuInit` without an exec sends nothing and exits
as it would have: it cannot use CUDA, and it holds a copy of its parent's
batch, locks and `atexit` handler without the parent's threads. Running the
closing flush there hung the child for good (500 kernels, then a fork, then
`exit`), so the handler and every send first check that the pid is still the
one that initialised (`a_child_forked_after_cuinit_exits_and_reports_nothing`). The library batches frames into messages of at most
60 KiB, one batch per CUPTI buffer and per flush, so a record costs no system
call of its own. Sends happen on CUPTI's thread, the flush thread or at exit,
never on a thread of the program's, and are bounded by a two-second send
timeout: a batch that could not be sent is dropped, counted, and reported in
the next one that is -- its records, and the counts of dropped and unfinished
records it carried; a kernel name it carried is sent again with the next
kernel of that name.

Every collector of a launch that goes through the library reads that one
socket (`Socket` in `kernel.rs`), the way the preload shim's readers share its
pipe: two `gpu.kernel` requests on one launch each see every record
(`two_requests_on_one_launch_each_read_every_record`), and a later CUPTI source
joins by adding its `REPORT_*` bit and subscribing.

`OMN_CUPTI_REPORT=0` loads the library and arms nothing: the negative control
a tracer's cost is measured against.

## The workloads, and what each guarantees

All in `tests/gpu_workload.py`: PTX loaded through `libcuda.so.1` with ctypes,
no toolkit. Each writes what it did to the file `OMN_GPU_TRUTH` names.

| Command | What it does | What it guarantees |
|---|---|---|
| `launches N` | N launches of `tiny`, one block of 32 threads, synchronised every 1024 | exactly N kernels; a launch rate set by the host, about 1 us of device time each: the load that fills CUPTI's buffers fastest |
| `forked N` | N launches of `tiny`, a synchronise, then a fork whose child exits at once through `exit`; the parent waits for it at most 30 s | exactly N kernels, all the parent's; a child that runs every `atexit` handler the parent registered, and a failure rather than a hang if one of them does not return |
| `compute N` | N launches of `spin`, about 2.4 ms each here | exactly N kernels whose time is arithmetic: no reads, one write per thread |
| `memory N MIB` | a memset and a host-to-device copy of MIB, N launches of `copy` (grid-stride, MIB read and MIB written), a copy back | exactly N kernels, 2 copies and 1 memset, each moving exactly MIB |
| `managed N MIB` | one managed buffer of MIB; the host writes all of it, then one `copy` kernel within it, N times | exactly N kernels, and pages migrating both ways between every two -- which are not kernels |
| `streams S N` | S non-blocking streams, N short `spin` launches on each, round-robin | exactly N kernels on each of S distinct streams, room on the device for all S at once |
| `selftimed N ITERS` | one warm-up and N launches of `selftimed`, each synchronised and its timings copied back | per launch: the kernel's own span from its blocks' `%globaltimer` reads, the SMs it ran on (`%smid`), its cycles (`%clock64`), and the host's `CLOCK_MONOTONIC` just before the launch and just after the synchronise |

## Ground truth, measured on the 2080 Ti

`tests/gpu_kernel.rs`, every number against the truth file:

- **Counts are exact** on every workload: 20 000 of 20 000 `tiny`, 50 of 50
  `spin`, 10 `copy` with 2 copies and 1 memset of exactly 8 MiB each, 10 kernels
  under managed memory and no migration counted as one, 4 streams of 25 each.
  Nothing dropped with the default buffers, including 50 000 launches.
- **Concurrency is kept**: on `streams 4 25`, 99 of the 100 kernels started
  before the previous one, on another stream, had ended.
- **Durations against the kernel's own span**, `selftimed 20 100000` (about
  1 ms each): CUPTI's duration is longer than the span the kernel measured on
  itself by **1.0 to 1.3 us**, every time -- the block dispatch before the first
  block's first instruction and the drain after the last block's last, which
  CUPTI's start and end include and `%globaltimer` inside the kernel cannot.
  That difference is not a clock error: the calibration's error on the same
  run was 68 ns. The test asserts the separation (span <= duration <= span +
  max(10 us, 1 %)), not the measurement. **This replaces the brief's "within
  the calibration's error", on purpose**: `ts_err_ns` bounds the conversion of
  CUPTI's clock into `CLOCK_MONOTONIC`, which a host bracket can measure;
  CUPTI's own placement of the device's work on its clock, and the dispatch
  and drain inside its duration, are upstream of any bracket and an order of
  magnitude larger. Folding a declared microsecond into `ts_err_ns` would make
  it a guess; the separation and the containment below are what bound the
  whole chain, and they are asserted.
- **Placement in time**: every converted kernel starts 7 to 10 us after the
  host's `CLOCK_MONOTONIC` before its launch and ends 1.2 to 1.9 us before the
  host's after its synchronise. Asserted as containment within the error.
- **The calibration**: CUPTI's clock on this machine is a wall clock
  (`1.79e18` ns, the epoch); a bracket of it between two monotonic reads is
  60 to 90 ns wide. The map's rate came out 1.0000002 to 1.0000015 over runs
  of a fraction of a second -- the bracket noise over so short a line, not a
  drift worth the name -- and the widest error any event carried was 68 to
  264 ns.
- **Drops are CUPTI's own count, and exact**: 50 000 launches with two
  buffers of 64 KiB (`OMN_CUPTI_BUFFERS=2 OMN_CUPTI_BUFFER_BYTES=65536`) kept
  41 263 and dropped 8 737 -- which add up to 50 000, so
  `cuptiActivityGetNumDroppedRecords` counts since its last call, not since
  the start. The run prints `LOSS: 8737 dropped` with the reason, and its
  summary turns into a lower bound (`n>=41263`). The test asserts kept +
  dropped = launched.
- **A program that never calls `cuInit`** reports no event and says so
  (`never loaded the CUDA injection library`), rather than an idle device.

Other numbers the same runs gave, for orientation: a 8 MiB `copy` takes 33 us
(about 500 GB/s of the 616 the card is rated for); the same kernel on managed
memory the host has just written takes 1.7 ms, because the migration is paid
inside the kernel's duration; a pageable 8 MiB host-to-device copy takes
650 us.

## Found here

- **A forced flush loses kernels' times.** `cuptiActivityFlushAll` with
  `CUPTI_ACTIVITY_FLAG_FLUSH_FORCED` hands back the buffer CUPTI is still
  filling, kernels that have not finished included, with their timestamps zero
  -- and those records are never delivered again. Measured: a forced flush
  every 50 ms under 500 launches of a 2 ms kernel returned 482 of the 500
  without a time. So the flush thread flushes unforced, a buffer comes back
  when it is full, and the last ones at exit, after the program's own
  synchronisation. The cost is latency: records arrive a buffer at a time
  (about a thousand kernels with the default 256 KiB buffers), which a live
  view of a slow program will show as nothing, then a batch. Records still
  arrive incomplete at exit when the program exits with work queued; they are
  counted in `dropped` with that reason, never placed at time zero.
- **`omn list` answered `gpu.kernel` "no" although CUPTI is in the loader's
  cache.** The probe was right: `cupti` read `available`, and the "no" was
  "no collector is written for `vendor:cupti`". With the collector registered
  the source is usable here. Nothing in the probe changed.
- **The workload's `cuLaunchKernel` signature was one argument short**:
  `sharedMemBytes` was declared as a pointer and the stream as a pointer to a
  pointer. It worked by accident of the calling convention while the stream
  was always null, and crashed the first time a stream was passed. Declared
  as the driver's header has it now.

## What is not measured, and why

- **Which thread launched a kernel.** The activity record does not say;
  correlating with the driver API's callbacks would, at a cost per launch,
  and is not done.
- **What it costs.** The reader's own cpu is on the cost line; what the
  library spends inside the program is not measured here, and the run says
  so. The bench stream measures it against the disarmed control.
- **Kernels launched through CUDA graphs, and unified-memory events.** Graph
  launches are reported as their kernels by CUPTI's concurrent-kernel
  activity, but no workload here launches a graph; migrations are a separate
  activity kind, not subscribed to.
- **The clock state.** A timing on unlocked clocks depends on what ran before
  it; reading whether they are locked belongs to the device stream, and the
  facts line here should carry it once it exists (requested).

## The clocks, and what a poll costs (nineteenth wave, stream device)

Measured on the same GeForce RTX 2080 Ti, driver 615.71.09, while other
streams used the board. `docs/scenarios/gpu.md` has the tables and the tests.

- **Clock state is read and never set.** Every run of an NVML source states,
  per device, before the run and after it: whether the shader clock is held at
  one frequency, the application clocks, persistence mode and the driver's
  clock event reasons. `collect::gpu::ClockState` is the type, serialisable, so
  the environment of a resolved file can carry it once that capture asks only
  when a GPU source is in the request (`nvmlInit` costs 3 to 5 ms, "Still
  open: the GPU in the environment capture" above). `timing_clock_facts()` is
  the line a GPU timing source adds to its facts.
- **NVML has no reader for locked clocks.** It can set them
  (`nvmlDeviceSetGpuLockedClocks`) and cannot say what was set; the
  applications-clocks event reason is documented as unused, and this board
  answers "not supported" for application clocks. The verdict comes from the
  driver's performance levels: locked only where every level runs the shader
  clock at one frequency. That a `-lgc` lock shows there is reasoned, not shown,
  so a range there is stated as "not shown locked", never as "not locked".
- **The driver's pace.** The energy counter and the draw move every 100 ms;
  the occupancy about once a second (1.0 to 1.13 s), by the driver's own
  sample timestamps. A faster poll reads the same sample again; the run counts
  those readings -- the counter by its value, which only rises, the occupancy
  by that timestamp, because an idle board reads the same value from new
  samples -- marks each event `driver_sample: repeat` or `new`, and says how
  often the figure moved. The one-second default poll is at or above the
  counter's and the draw's refresh, and just under the occupancy's: at the
  default an occasional occupancy reading is a repeat, counted and said.
- **What a call costs** is cpu, not waiting: the driver spins. From 6 µs
  (persistence mode) and 12 to 48 µs (temperature) to 0.8 to 2.1 ms (one clock
  domain) and 2.8 to 7 ms (the energy counter, the draw). A `gpu.power` read is
  6 to 11 ms of a core.
- **Reading the counter often makes it under-count** -- for every reader of
  the board, across processes. Idle, the counter read at its two ends gives
  23 W; read every 100 ms, 9 to 14 W; every 10 ms, 3 W, while the draw read
  beside it rises to 36 W. This revises "integrating sampled power is not
  energy, and polling faster makes it worse" above: at least part of the 20 Hz
  gap is the counter starved by the poll, not the integral overstating. A
  `gpu.power` poll whose counter falls below half the draw says so; energy is
  `--mode count`.

# Inside a kernel: `gpu.counters` and `gpu.profile`, through CUPTI (nineteenth wave, stream profiler)

`gpu.kernel` says how long each kernel took; these two say what happened inside
one. Both go through the same injection library and the same socket as
`gpu.kernel` (`cupti/src/profiler.rs` and `cupti/src/pcsampling.rs` in the
library, `src/collect/gpu/counters.rs` and `src/collect/gpu/profile.rs` in
`omn`), each arming its own bit of `OMN_CUPTI_REPORT`, and both serve a
launched program only, for `gpu.kernel`'s reason. Nothing is wrapped: no
`ncu`, no `nsys`; `vendor:ncu` left the chain.

## `gpu.counters`: counters per launch, and a verdict per counter

CUPTI's range profiler, set up on every context the program creates, in
**kernel replay** with **automatic ranges**: every launch is a range, run as
many times as the counters need passes (one, for the default set on this
chip). Every counter of every launch comes back as a frame; `omn` holds them
per kernel and counter and, at the end, emits one event per kernel and
counter: `value` (the median over the launches), `min`, `max`, `spread` (the
interquartile range over the median), `outliers` (launches more than 10 % from
the median), `launches`, `disturbed`, `passes` and a `verdict`. The default
counters are `smsp__inst_executed.sum`, `dram__bytes_read.sum`,
`dram__bytes_write.sum`, `lts__t_bytes.sum` and `l1tex__t_bytes.sum`;
`OMN_CUPTI_METRICS` names others, and a name the chip does not have is said and
the rest measured.

**The verdict is measured, per counter, per run.** `reproducible here` when
the middle half of the launches is within 10 % of the median, `not dominated
by this workload` when it is not, `measured once` for one launch. Held to two
workloads that disagree about the same counter (`tests/gpu_counters.rs`):

| Kernel | `smsp__inst_executed.sum` | `dram__bytes_read.sum` |
|---|---|---|
| `copy`, 64 MiB, memory-bound | 3 167 488 on every launch | 67.1 to 67.4 MB (the 64 MiB it reads, and a little), spread under 5 % end to end |
| `spin`, compute-bound | 353 620 672 on every launch | 0 to 3.9 MB, spread 180 to 340 % |

**Kernel replay destroys the timeline**, so a run that asks for counters and
for `gpu.kernel` is refused before the program runs, with the reason
(`counters_beside_kernel_times_or_pc_sampling_are_refused`); so is one with
`gpu.profile`, because CUPTI cannot run its range profiler and its PC sampling
in one process. The refusal is made at attach, where every reader of the
launch has added its bit and the program is still parked at its gate.

**Ranges pair with launches only when nothing can shift them.** Ranges come
back in launch order and carry no launch's identity, so a batch is paired by
order only when it holds as many ranges as launches the library recorded and
no range launched fewer blocks than its launch's grid. A range with no
recorded launch -- a CUDA graph's kernel node, whose launch call the library
does not follow -- anywhere in the batch would otherwise shift every launch
after it onto its neighbour's counters, and the witness would at best call the
shifted launches disturbed, at worst (equal grids) pass them clean. Such a
batch is not reported: its launches or ranges, whichever are more, are counted
as unpaired (`ranges_and_launches_pair_only_when_their_counts_agree`, a unit
test; no workload here launches a graph).

**When.** Every event stands for all of its kernel's launches: it is stamped
when the first launch call was entered, in the program's `CLOCK_MONOTONIC`,
and its `ts_err_ns` reaches to when the last launch's counters reached `omn` --
a window that holds every launch's work however late the library decoded it
(about 100 ms for `memory 6 64`; `ts_err_ns` saturates at 4.29 s, the width of
the field).

**Clocks are left alone.** CUPTI 13.4 locks the device's clocks only when asked
(`cuptiClockControlLock`), and the library never asks; the clock state is in
the facts, as for every GPU timing source.

### Found: the device counts other processes' work, and a wrong count can repeat

The range profiler's counters are the device's: another process's kernels
running in a launch's window are counted with the launch. Measured while
another worktree looped GPU programs that each lived under a second: the
`copy` kernel's instruction count read 286 220 778 instead of 3 167 488 --
**on every launch of the run, by the same amount** -- so the wrong count
repeated bit for bit and a spread alone called it reproducible. NVML's process
list, asked once a second, did not see the other programs at all.

So the library always collects a **witness**, `sm__ctas_launched.sum`, the
blocks the SMs launched in the window, and compares it with the grid the
launch call asked for (read from `cuLaunchKernel`'s parameters, or
`cuLaunchKernelEx`'s config): 272 for the `copy` grid when the window was the
launch's alone, 892 to 1 032 when it was not. A launch with more blocks than its
grid is `disturbed`, counted and left out of every counter; a kernel disturbed
on every launch is `disturbed on every launch`, with no value of its own.
Fewer blocks than the grid is not a disturbance -- other work only adds -- but
a range that is not the launch's, and then none of its batch is reported.
Work that launched no block in the window -- a long kernel started before it
-- escapes the witness and shows as `outliers` or a spread, which is why the
spread is the middle half's: a few such launches do not turn a count that
repeats into one that does not, and the launches they disturbed are counted
beside the verdict.

## `gpu.profile`: where the cycles go, line by line

CUPTI's PC sampling, enabled and configured on every context the program
creates, in **continuous** mode -- the program's kernels are not serialised --
every `2^12` cycles of each SM (`OMN_CUPTI_PC_PERIOD`, 5 to 31). The library
drains CUPTI's per-PC aggregates every 256 launches, at every module load, when
a context is destroyed and at exit, keeps every module it saw by the CRC CUPTI
tags program counters with, and resolves each program counter to its file and
line with `cuptiGetSassToSourceCorrelation` when the module has line
information. One event per program counter and stall reason per drain,
weighed by `samples`, stacked `kernel;file:line;stall` (or
`kernel;+0x<offset>;stall`), stamped at the drain in the program's own
`CLOCK_MONOTONIC` with the window since the previous drain as `ts_err_ns`.
`-e stack` is a flame of the device's cycles; `topk:10@source` ranks lines.

CUPTI reports every stall reason twice, the second time restricted to the
samples where no instruction issued (`..._not_issued`): a subset, left out so
a sample counts once. With it the samples reported per program counter add up
to CUPTI's own count exactly, less its samples in kernels that are not the
program's (2 412 of 2 227 167 on one run).

### Ground truth

`tests/gpu_inside.py hotloop N ITERS`: a kernel whose loop body is sixteen
dependent multiply-adds on line 20 of a source that only its `.loc` directives
name, loop control on line 21, setup on line 10 and the store on line 30,
JIT-compiled with `CU_JIT_GENERATE_LINE_INFO`; `hotloop-bare` is the same PTX
without the directives. Measured here, 6 launches of 100 000 turns:

| Line | Samples | Share |
|---|---|---|
| 20, the dependent chain | 640 027 | 95.9 % |
| 21, loop control | 27 322 | 4.1 % |
| 10, setup | 101 | 0.02 % |
| 30, the store | about 10 | 0.002 % |

**A configuration CUPTI accepted and did not apply is caught by its effect.**
Every attribute's own status is checked as well as the call's; the period and
the mode CUPTI applied are read back and a difference said; the launches the
library saw are sent with every drain, and a run whose kernels took no sample at
all is a lost read, not an empty profile. One kernel with no sample among
others that have some is not: a kernel shorter than the period on every SM is
expected to take none. CUPTI's own count of the program's samples is held to
the samples the program counters carried, and a shortfall past what CUPTI said
it dropped is counted as lost, with the reason
(`samples_cupti_counted_and_no_program_counter_carried_are_lost`). A period
setting outside 5 to 31 is said, and the default used. The period is held by its effect:
the same work took 667 450 samples at `2^12` and 166 880 at `2^14`, a ratio of
4.0 (`the_sampling_period_asked_is_the_one_applied_by_its_effect`).

**The samples are held to the kernel's own cycles.** `selftimed` -- one block
per SM, each reading `clock64` before and after its work and writing `%smid` --
says how many cycles each SM spent in it; every SM samples once per `2^n` of
its cycles, so a launch should take its blocks' cycles times its SMs over `2^n`.
Measured: 233 053 samples against 211 743 for the ten recorded launches, the
eleventh (a warm-up of the same length) making the ratio 1.0006; 1.0014 at
`2^14` (`the_samples_are_the_kernel_s_own_cycles_over_the_period`, held within
10 %).

A kernel without line information is placed by offset and named in the facts,
and the offset is the hot loop's: the same PTX with line information samples
hottest at the same offset, on line 20
(`a_kernel_without_line_information_is_placed_by_offset_and_named`). `gpu.profile`
and `gpu.kernel` share one launch: both read every frame meant for them, and
neither counts the other's as unknown (`a_profile_and_kernel_times_share_one_launch`).

## What is not measured, and why

- **What either costs the program.** The bench stream's, against the
  library loaded and disarmed. A replayed launch costs at least its own run
  once per pass; PC sampling's cost was not measured.
- **Launches through a CUDA graph.** The library follows the launch entry
  points it knows the parameters of; a graph's kernels are ranges with no
  launch call to pair with, and the batch that holds them is counted as
  unpaired, none of it credited to the wrong kernel.
- **Another vendor's reading.** `ncu` and `nsys` are present here and could
  cross-check an instruction count or a hot line; the self-built references
  (the two workloads' disagreement, the known hot line) were used instead.
- **Multi-device.** Everything was measured on one 2080 Ti (TU102, CUPTI
  13.4); the code keys every context by its device, unexercised past one.

# What a GPU tracer costs (nineteenth wave, stream bench)

Measured in `docs/bench.md`, "The GPU tracers, nineteenth wave", on three
regimes (`examples/bench/gpu.c`) and against a negative control -- the
injection library loaded by the driver and disarmed:

| backend | launch-bound (a kernel every 2 us) | compute- or memory-bound (a kernel every ms) | band |
|---|---|---|---|
| `gpu.kernel` through `vendor:cupti` | +67 % of the time, +83 % of the cpu, against the control | under 1 % | `extreme` (was `med`) |
| `gpu.api` through `preload` | +11.3 % | 3 % or less | `high` (was `low`) |
| the NVML polls | what the poll reads | the same | unchanged |
| the library loaded, disarmed | inside the floor | inside the floor | - |

CUPTI's price is paid inside each launch, by the launching thread, about
1.3 us a launch: with the workload's other threads kept off its cpu, that
thread's own cpu grew by 66 %, and every other thread spent 17 ms more on
other cpus. `nsys`, CUPTI underneath too, costs the launching thread about as
much (+74 %). A band per backend holds the worst regime, and
each source's caveat names it. `gpu.counters` and `gpu.profile` are pending in
the record until their collectors land.
