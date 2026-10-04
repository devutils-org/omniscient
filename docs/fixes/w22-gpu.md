# Wave 22, stream gpu: handoff

Branch `feat/w22-gpu`. `omn` against `nvidia-smi` (query and `dmon`), `nsys`
and `ncu`, on the development machine's RTX 2080 Ti (driver 615.71, nsys
2026.3.2, ncu 2026.3.1). The verdict table is `docs/replaces/gpu.md`.

## What landed

- `catalog/references/gpu.yaml`: 21 pairs. nsys (8): launches per kernel and
  device time, launch-bound and memory-bound; copy count and time; driver
  calls (`cuLaunchKernel`, `cuMemAlloc`, `cuMemcpyDtoH`) counted. ncu (5):
  `smsp__inst_executed.sum` on two kernels, `sm__ctas_launched.sum`,
  `l1tex__t_bytes.sum`, `dram__bytes_read.sum`. nvidia-smi (8): power
  (query and dmon), shader clock (query and dmon), utilisation (query and
  dmon), temperature, per-process VRAM.
- `tests/reference/gpu.rs`: six parsers (`nsys_gpu_sum`, `nsys_api_sum`,
  `ncu_metrics_csv`, `smi_query_gpu`, `smi_dmon`, `smi_compute_apps`), every
  value in omn's unit and omn's naming (nsys's `[CUDA memcpy
  Host-to-Device]` is `[memcpy HtoD]`, `cuMemAlloc_v2` is `cuMemAlloc`). 12
  parser tests on 8 captures under `tests/reference/captures/gpu/`, two of
  them the mixed stream a same-run pair hands over (samples and the
  workload's notes on one standard error). One ignored test,
  `gpu_pairs_hold_on_this_machine`, drives the pairs with the family's
  parsers and `compare.omn_jq` (`OMN_GPU_PAIRS=<text>` narrows it).
- `tests/reference/workloads/gpu/smi_during.sh`: runs a workload and samples
  nvidia-smi beside it (`gpu`, `dmon`, `apps`), samples on standard error so
  omn can launch it and keep its JSON on standard output.
- `tests/reference/workloads/gpu/linked.sh`: compiles `examples/bench/gpu.c`
  with `-lcuda` once into `target/reference/gpu` and runs it; `gpu.api` sees
  only calls bound through the dynamic linker.
- `docs/replaces/gpu.md`: 20 rows, one per tool and use, each with its pair
  or why none.

## Shown vs reasoned

**Shown** (measured here, by the ignored test and by hand, 2026-10-04):

- Exact on both sides: kernel launches (5000/5000, 5/5), copy count, driver
  call counts, instruction counts (808 192; 353 620 672), blocks (272), L1
  bytes (33 554 432), per-process VRAM (432 013 312 bytes).
- Within: launch-bound kernel time 0.02-0.3 %; memory-bound kernel time
  under 1 % in five runs of six, 7.1 % once; DRAM bytes 0.1-0.4 %; power
  0.4-5.6 %; shader clock under 0.4 %; utilisation 0.00-0.08 absolute;
  temperature under 0.4 C.
- Two full runs of all pairs: 19/21, then 20/21; each miss changed as
  `docs/replaces/gpu.md` says, and the changed pairs passed in further runs
  of their own (utilisation four times, the copy's time three).
- Same run for nvidia-smi: omn launching `smi_during.sh` works, and its
  `--program-stderr` file is what the parser reads.

**Reasoned, not shown**:

- That nsys and ncu cannot share a process with omn's CUPTI library: from
  the injection mechanism (both use `CUDA_INJECTION64_PATH`); not tried.
- Behaviour of the pairs on a machine without a GPU: each pair `requires`
  `nvidia-smi` (and `nsys`, `ncu`, `cc`), so a runner that honours
  `requires` skips them; not run without them.

## Tests

- `cargo test --test reference gpu`: 12 passed, 1 ignored.
- `cargo test --test reference`: 17 passed, 1 ignored (with io's merged).
- `cargo test --test reference gpu_pairs -- --ignored --nocapture`: about
  70 s; see above.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
  `cargo fmt --check`: clean.
- `cargo test --test cli` (the document and index checks): 28 passed.

## Not done, and why

- nsys's per-kernel distribution (Avg, Med, Min, Max, StdDev): omn has no
  emit that gives it per kernel (gap 2), so no pair.
- Host time per driver call against nsys: nsys's own tracing is inside the
  call; not a reference (two nsys runs 1.46 and 3.47 ms for one copy).
- `nsys stats -r cuda_gpu_mem_size_sum` (bytes per copy), device-wide
  `memory.used`, dmon's `mclk` and `mem`: not paired, for time.
- ncu sections, nsys NVTX, dmon `-s v/t/e`: omn has no source (gaps 5, 6,
  9); rows in the table say so.
- `gpu.profile` against ncu's source counters: different measures (warp
  stall samples by PC against instructions per line); no pair, by design.

## Gaps in omn (each a line of work, not fixed: `src/` is not this stream's)

1. `gpu.mem` on a launched program reads the launched pid only: under a
   wrapper (`sh` running the CUDA program) the summary is 0 for a process
   tree holding 412 MiB. It should follow the launched program's
   descendants, as the cpu sources do.
2. No per-key distribution: `topk:N@kernel` gives count and total per
   kernel, but `summary` and `hist` take no `@key` (`summary@kernel` is
   refused), so nsys's per-kernel Avg/Med/Min/Max needs `-e raw` and a
   script.
3. `gpu.api` does not interpose `cuMemsetD8`, `cuModuleLoadData`,
   `cuCtxCreate`, which nsys's `cuda_api_sum` lists.
4. `gpu.api` is blind to programs that reach the driver through `dlsym` or
   `cuGetProcAddress` (ctypes, the CUDA runtime), a known caveat; a CUPTI
   callback-API backend would see every call, as nsys does.
5. No NVTX source: nsys's ranges have no omn equivalent.
6. A launch's own facts as values -- grid, block, registers, shared memory,
   theoretical occupancy -- which ncu's launch statistics and occupancy
   sections give.
7. `gpu.freq`'s memory clock is an attribute of each raw event
   (`memory_hz`), not a value `summary` aggregates, so dmon's `mclk` has no
   summary equivalent.
8. `gpu.engine_busy` has no process key: `nvidia-smi pmon`'s per-process
   busy share has no equivalent; nor has dmon's memory-controller `mem`.
9. No source for PCIe throughput, throttle reasons as values, ECC and
   replay counters (`dmon -s t`, `-s v`, `-s e`).

## Privileged runs wanted

None. Every pair runs unprivileged here: nsys, ncu (performance counters
are not restricted to admin on this driver) and nvidia-smi. On a machine
where ncu reports `ERR_NVGPUCTRPERM`, the grant is the driver option
`NVreg_RestrictProfilingToAdminUsers=0` (a module option and a reboot, the
user's call), or running ncu as root.

## Decision lines

- nvidia-smi pairs observe the same run: omn launches
  `smi_during.sh <mode> <workload>` with `--program-stderr`, and the sampler
  writes to standard error.
- nsys and ncu pairs alternate runs, comparing counts exactly and
  durations with the measured spread as the band.
- Host-side call durations are not compared against nsys: the tool's
  tracing is in the duration.
- Parsers convert to omn's units and names; a pair never converts.

## Needs from another stream

- **reference** (the runner): read `same_run` (`omn_launches_tool` as io
  defines it; `alternate` as the default), hand the `--program-stderr` file
  to the parser as stderr, and evaluate `compare.omn` with the selectors
  these pairs use: `topk[kernel=copy].events`, `topk[kernel=\[memcpy
  HtoD\]].events` (a glob with escaped brackets), `raw[kernel=copy,counter=
  smsp__inst_executed.sum].value` (an event selected by two attributes, its
  `value` attribute), `rate[*].per_second`, `summary[*].mean`. Each pair
  also carries `compare.omn_jq`, the same value as a jq expression, which
  is what the family's ignored test uses; a runner may prefer it.
- **reference**: the gpu pairs drive the GPU for about 70 s together, and
  two GPU workloads at once disturb each other's counts (`gpu.counters`
  flags the launches, ncu does not); run them serially with nothing else on
  the GPU.
- **reference**, `.github/expected-skips*.txt`: on CI (no GPU) every gpu
  pair skips for `nvidia-smi` missing.
