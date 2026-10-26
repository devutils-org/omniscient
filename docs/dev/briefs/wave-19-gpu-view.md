# Brief — inside the GPU, and a view of it all

The nineteenth wave has two halves that touch different files and run side by
side.

**The GPU half**: the two GPU sources the catalog has named since the first
wave and nothing collects, `gpu.kernel` and `gpu.counters`, a new one,
`gpu.profile`, and what every GPU figure needs to be read honestly -- its clock
domain, the state of the device's clocks, and its cost measured against a
control. On NVIDIA the device has no kernel interface below the driver
(`docs/families/gpu.md`, "The asymmetry is the design"), so this wave goes
through the vendor's own library, opened at runtime, the way `vendor:nvml`
already does: the library is the interface, and no tool is wrapped.

The AMD and Intel kernel paths are not in this wave: no such card is in the
machine, and they wait for one.

**The view half**: `omn top`, an interactive terminal view of everything `omn`
measures, and with it ways of showing the data that no system monitor has -- a
flamegraph in the terminal, the cost sheet of a process or of one request
across every resource, where a run's wall-clock went, latency as a heatmap over
time. Every view is **an experiment**: they are all built, looked at on the
same recorded runs, and the user keeps what proves interesting. A view is not
argued for up front; it is judged on screen.

It runs as **fourteen streams**: three at once (the GPU's two first streams and
the view's core), then eleven that need them merged (the GPU's last two and
nine views).

Read `CLAUDE.md` first -- **including "How much to test, and when"** -- then,
for a GPU stream, `docs/families/gpu.md`, `docs/scenarios/gpu.md` and
`catalog/gpu.yaml`; for a view stream, `src/encode.rs`, `src/render.rs`, the
flight recorder's format and `docs/view/README.md` once core has written it;
then your section here. The earlier briefs are in `docs/dev/briefs/`.

**This is the only brief.** What comes after this wave is written in
`docs/dev/STATE.md` under "What remains", and nowhere else.

---

## Decided before this wave

- **CUPTI is reached from an injection library**, a workspace member like the
  preload shim, loaded into a launched target through `CUDA_INJECTION64_PATH`.
  CUPTI's activity and profiling APIs only work inside the process that owns
  the CUDA context, so `gpu.kernel`, `gpu.counters` and `gpu.profile` serve a
  **launch** target and refuse a `pid` target with that reason. The catalog's
  `targets` lines change to say so.
- **Nothing is wrapped.** `gpu.counters` reads CUPTI's profiler API from the
  injection library; it does not run `ncu` and parse its output. `vendor:ncu`
  is replaced by the CUPTI backend in the chain. `ncu` and `nsys` may appear in
  a test as independent second readers, as `nvidia-smi` already does.
- **Lost records are counted and said.** CUPTI drops activity records when its
  buffers fill, and reports how many only if asked. Every `gpu.kernel` run asks,
  carries the count into the run's losses, and a run that lost records says so
  beside the numbers. A flush thread keeps the buffers drained; the drop count
  under a launch-bound load is a test, not a hope.
- **GPU timestamps carry their error.** CUPTI's clock is converted to
  `CLOCK_MONOTONIC` by a calibration measured at the start and end of the run,
  and every event carries the residual as a non-zero `ts_err_ns`.
- **The device's clocks are reported, never changed.** Locking a GPU's clocks
  needs root and does not survive a reboot, and an unlocked GPU makes a timing
  depend on what ran before it. `omn` does not change machine state: it reads
  whether the clocks are locked (and persistence mode), records it in the run's
  environment, and a GPU timing taken on unlocked clocks says so in its facts
  line.
- **Counter reproducibility is measured per run, not declared.** A counter is
  reproducible only when the workload dominates the quantity counted: the same
  DRAM counter is stable on a memory-bound kernel and noise on a compute-bound
  one. So `gpu.counters` takes each counter over several passes and reports its
  spread beside its value, with a verdict per counter (`reproducible here` /
  `not dominated by this workload`), never a fixed list of good counters.
- **`gpu.profile` is a new source**: where time goes inside a kernel, by CUPTI's
  PC sampling, as `cpu.profile` says where time goes on the cpu -- a sample is a
  program counter in a kernel with its stall reason, resolved to the kernel's
  source line when the module carries line information.
- **A GPU tracer's cost is measured against a negative control**: the same run
  with the injection library loaded and disarmed. Without it, the dynamic
  loader's own cost is published as the tracer's.
- **Ground truth stays independent and toolkit-free.** The workloads are PTX
  driven through `libcuda.so.1` opened at runtime, like `tests/gpu_workload.py`
  today: no `nvcc` needed to run the suite. A kernel that instruments itself --
  `%globaltimer`, `clock64` and `%smid` written per block -- is the reference
  `gpu.kernel` and `gpu.profile` are held to.

- **The view is a projection, never a collector.** `omn top` reads the same
  event stream the encoders read; no view opens a source, polls a file or
  starts a program of its own. A view that needs data no source gives files the
  request against that source's owner.
- **Unused, it costs nothing.** The view is an opt-in bonus: a run without
  `omn top` pays exactly what it pays today, and nothing in the collectors
  changes for the view's sake. When it runs, its own cpu is on screen beside
  the target's, because that is what `omn` does for every collector.
- **Every view runs on a recording.** `omn top --replay <run>` draws any view
  from a recorded run (the flight recorder's files or a `results.json`) as it
  draws a live one, so all views are compared on the same data, tests are
  reproducible, and a demonstration is the same every time. `omn top --gallery
  <run>` steps through every view on one run.
- **Trust is drawn, not footnoted.** A figure with losses, a non-zero time
  error or low determinism is drawn dimmed, with its loss or error bar beside
  it; a source that was refused shows its reason where its panel would be. One
  style for this, defined by core, used by every view.
- **One binary carries the capabilities.** The view lives in a workspace member
  (`view/`, ratatui and crossterm), linked into `omn` as the `top` subcommand,
  so the privilege model stays the one wave 18 settled.

Each of these moves into `docs/DECISIONS.md`'s settled table, with its reason, in
the stream that lands it.

---

## How you test, and how you read

- **While you work**: only your module and your test files. Never `make check`
  while iterating.
- **Before your last commit**: one `make check`; if red, one
  `cargo test --workspace --no-fail-fast`. A red test outside your paths that is
  in the flaky tally: rerun it alone once.
- **`cargo fmt` before `make`, never after.** `make` once per build, before
  eBPF tests.
- **Read by range**: `grep -n`, then the lines you need.
- **A view is tested on its snapshot**: ratatui's test backend draws the view
  from a fixture recording into a text buffer, and the test compares it with a
  snapshot under `docs/view/gallery/`. The snapshot is also what the user looks
  at to judge the view, so it is drawn from a realistic run, not a toy.
- **The GPU is one device shared by every stream.** A test that times the GPU
  asserts separations, not thresholds, and says in its failure message what
  else held the device (`nvidia-smi --query-compute-apps`). Do not lock clocks,
  do not change persistence mode, do not set `RmProfilingAdminOnly`: list it in
  your handoff with the exact command; the coordinator runs it alone.
- **The CI has no GPU.** Every GPU test skips there with the machine's reason
  first, through a pattern already in `.github/expected-skips*.txt` or one you
  add with its reason.
- **Disk**: each worktree builds without incremental compilation (its untracked
  `.cargo/config.toml`; never commit it).

## Where you work, and how you hand off

Each stream has its own worktree, `/home/moha/repos/omniscient-w19-<stream>`,
on `feat/w19-<stream>`. The handoff is `docs/fixes/w19-<stream>.md`: what
landed, shown versus reasoned, what you could not do and why, the privileged
runs you want, your decision lines, and the requests to other files under
"Needs from another stream".

---

## The streams

| Stream | Owns | Phase |
|---|---|---|
| **cupti** | the injection library (new workspace member), `src/collect/gpu/kernel.rs` (new), `src/clock.rs` (the GPU calibration), the `gpu.kernel` entry of `catalog/gpu.yaml`, `tests/gpu_workload.py`, `tests/gpu_kernel.rs` (new), the root `Cargo.toml`'s members line, `Makefile` and `scripts/release.sh` (installing the library) | A |
| **device** | `src/collect/gpu/{nvml,mod}.rs`, the `vendor:nvml` lines of `catalog/gpu.yaml`, `tests/{family,scenario}_gpu.rs`, `docs/scenarios/gpu.md` | A |
| **profiler** | the injection library's profiler and PC-sampling modules, `src/collect/gpu/{counters,profile}.rs` (new), the `gpu.counters` and `gpu.profile` entries, `tests/gpu_counters.rs` (new) | B, after cupti |
| **bench** | the `cost:` lines of `catalog/gpu.yaml`, `docs/bench.md`'s GPU section, `tests/bench_*.rs`, `examples/bench/**` | B, after cupti and device |
| **core** | `view/` except `view/src/views/*.rs`, the `top` subcommand in `src/cli.rs` and `src/main.rs`, the root `Cargo.toml`'s members line (after cupti's), `docs/view/README.md`, `tests/view_*.rs` fixtures | A |
| **system** | `view/src/views/system.rs`, its snapshots and `docs/view/system.md` | B, after core |
| **flame** | `view/src/views/flame.rs`, idem | B, after core |
| **ledger** | `view/src/views/ledger.rs`, idem | B, after core |
| **request** | `view/src/views/request.rs`, idem | B, after core |
| **timebar** | `view/src/views/timebar.rs`, idem | B, after core |
| **heatmap** | `view/src/views/heatmap.rs`, idem | B, after core |
| **lanes** | `view/src/views/lanes.rs`, idem | B, after core and cupti |
| **wake** | `view/src/views/wake.rs`, idem | B, after core |
| **flows** | `view/src/views/flows.rs`, idem | B, after core |

`docs/families/gpu.md` takes one section per stream, appended; the coordinator
merges them. `docs/DECISIONS.md`, `docs/dev/STATE.md`, `docs/ROADMAP.md` and the indexes are the
coordinator's.

### cupti -- every kernel, its time, and what was lost

- The injection library: subscribe to kernel, memcpy and memset activity,
  drain the buffers on a flush thread, ship records to `omn` the way the
  preload shim ships its own (a shared ring, bounded, losses counted).
- `gpu.kernel` served through `vendor:cupti` for a launch target: kernel name,
  device, stream, grid and block, start and end converted to `CLOCK_MONOTONIC`
  with `ts_err_ns`. A `pid` target refused with its reason.
- The workloads, PTX through `libcuda`: a **launch-bound** load (thousands of
  microsecond kernels), a **compute-bound** one, a **memory-bound** one, a
  **managed-memory** one and a **multi-stream** one, each with what it
  guarantees in `docs/families/gpu.md`'s table; and the **self-timing kernel**
  that writes `%globaltimer`, `clock64` and `%smid` per block.
- Ground truth: kernel count exact against the workload's own count; each
  kernel's duration against the self-timing kernel's own span, within the
  calibration's error; the drop count under the launch-bound load reported,
  non-zero when the buffers are made small enough to drop, and the run saying
  so.
- Why `omn list` answers `cupti` missing on this machine although
  `/opt/cuda/lib64/libcupti.so` is in the loader's cache: fix the probe if it is
  wrong, and say in the handoff which.

### device -- the clocks, and what a poll costs

- The run's environment records, per GPU: whether the application or locked
  clocks are set, persistence mode, and the clock reasons NVML gives
  (`nvmlDeviceGetCurrentClocksEventReasons`); a GPU timing on unlocked clocks
  says so in its facts line.
- The driver refreshes NVML's readings at its own rate. Measure it here; a poll
  faster than it repeats readings, so repeated readings are counted and said,
  and the default poll is set to what the driver can actually answer.
- NVML's cost to the collecting process, measured per call kind, into the
  caveats.

### profiler -- inside a kernel (phase B)

- `gpu.counters` through CUPTI's profiler API from the injection library:
  instruction, memory and cache counters per kernel, each taken over several
  passes, its spread and its verdict beside it. A test holds the verdict to
  the workloads: instruction counts reproducible on both, DRAM traffic
  reproducible on the memory-bound kernel and refused as not dominated on the
  compute-bound one.
- `gpu.profile` through CUPTI's PC sampling: samples per kernel, PC and stall
  reason, resolved to a source line when the module has line information.
  Ground truth: a kernel whose hot loop is known (a long dependent chain in one
  place, nothing elsewhere) puts most samples there. The PC-sampling API's
  ordering constraints are where it fails silently: every call's result is
  checked and a configuration CUPTI accepted but did not apply is detected by
  its effect, not by the status code.
- Neither destroys `gpu.kernel`'s timeline silently: a run that asks for
  counters (kernel replay) and for kernel times is refused, or says the times
  were taken under replay.

### bench -- what a GPU tracer costs (phase B)

- Each GPU backend's cost on each workload regime, against the wall clock alone
  and against the negative control (library loaded, disarmed); the bands in
  `catalog/gpu.yaml` set from the table, and the regime named where the cost
  depends on it (a tracer's cost on a launch-bound load and on a compute-bound
  one can differ by orders of magnitude).
- `nsys` measured once as a reference reader, not served.
- `tests/bench_*.rs` hold the GPU bands to the table, as for the other
  families.

### core -- the frame every view draws in

- `omn top` live (on a target or the system, the same targets as `omn trace`),
  `--replay <run>` and `--gallery <run>`; the layout, the navigation between
  views and into a process or a request, the help line.
- The data layer every view reads: the event stream, live or replayed, with
  each figure's losses, `ts_err_ns`, determinism and refusals carried through.
- The trust style (dimmed, error bars, refusal in place) and the on-screen cost
  of the view itself.
- The registry with **all nine views already registered as stubs**, so each
  view stream fills its own file and touches nothing else.
- Fixture recordings for the views to draw: a real run of a few cases
  (`cases/`), recorded once, small enough to commit, and the snapshot harness.
- Which sources each view needs, listed in `docs/view/README.md` so the view
  streams do not rediscover it.

### system -- the overview

A btop-like front page: cpu, memory, disk, network, GPU and power panels with
sparklines and gauges, the process list beside it, every figure in the trust
style, and the view's own cost in a corner.

### flame -- a flamegraph in the terminal

From `cpu.profile` and `sched.offcpu` stacks: an icicle that zooms into a frame
and back out, search by name, on-cpu and off-cpu side by side, and the
**difference of two runs** (`omn top --replay <a> --against <b>`), red where the
second is slower and blue where it is faster, from the triage's own diff.

### ledger -- what a process costs

One process's cost sheet across every resource `omn` measures: cpu time and
its split, memory held and faulted, bytes read and written and their latency,
network volume and retransmissions, GPU time and memory, energy where it can be
attributed -- each line with its losses and trust, and the sources that could
not be served said in place.

### request -- what one request costs

The same sheet for one unit of work -- an HTTP request, a job, a transaction --
from `net.request` and the join engine: a waterfall of its life by state (on
cpu, waiting to run, blocked on disk, on a lock, on the network) and its share
of each resource. Held to the `http-server-*` cases, whose requests' causes are
known.

### timebar -- where the wall-clock went

The triage's decomposition drawn as one bar per thread or task: on cpu, run
queue, I/O, lock, sleep, child -- with the first suspect named on it, the same
one `omn latency` names.

### heatmap -- latency over time

Time across, latency up, density in colour, for any latency source
(`io.latency`, `sched.runqlat`, `net.socket`, `gpu.kernel`): the bimodal regimes
and the spikes a histogram flattens. Lost events shaded where they were lost.

### lanes -- threads side by side

One lane per thread, its states along time, Perfetto-like but in the terminal,
with a lane per GPU stream once `gpu.kernel` is merged, so a kernel and the
host thread that waited for it line up.

### wake -- who woke whom

The wake chain from `sched.wakeup` as a tree: from a blocked thread up to what
finally unblocked it, with the time spent at each link.

### flows -- where the bytes went

Bytes as flows: process -> socket -> interface, and process -> file -> device,
widths in proportion, each edge with its rate and its losses.

---

## Exit criteria for the wave

- `gpu.kernel` served on this machine, its kernel count exact and its
  durations within the calibration's error of the self-timing kernel, its
  losses counted and said.
- `gpu.counters` with a measured verdict per counter, held to two workloads
  that disagree about the same counter.
- `gpu.profile` putting the known hot loop first.
- Every GPU timing carries its clock state; nothing changes it.
- The GPU bands set from a table measured against a negative control.
- `omn top` live, `--replay` and `--gallery`, costing nothing when unused.
- All nine views drawn on the same fixture runs, each with its snapshot in
  `docs/view/gallery/` and a one-line verdict from its stream (what it shows
  that nothing else does, and what it does not) for the user to judge.
- `make check` green at each merge; the CI's GPU tests skipping with the
  machine's reason.
