# What omn replaces: the `gpu` family

The pairs are in `catalog/references/gpu.yaml` (21), the parsers in
`tests/reference/gpu.rs`: the tools' (`nsys_gpu_sum`, `nsys_api_sum`,
`ncu_metrics_csv`, `smi_query_gpu`, `smi_dmon`, `smi_compute_apps`) and two
of `omn`'s output (`omn_topk`, `omn_counters`), each tested on an output
under `tests/reference/captures/gpu/`. The workloads are the GPU scenarios'
own (`tests/gpu_workload.py`: kernels, launches, grids and allocations
decided by the program, through `libcuda` and no toolkit), plus two wrappers
under `tests/reference/workloads/gpu/`: `smi_during.sh` runs a workload and
samples `nvidia-smi` beside it, and `linked.sh` runs the bench's
`examples/bench/gpu.c` linked with `-lcuda`, the only kind of program
`gpu.api` sees. Measured on the development machine: an RTX 2080 Ti, driver
615.71, nsys 2026.3.2, ncu 2026.3.1, clocks not locked.

How the runs are paired. `nvidia-smi` launches nothing, so `omn` launches
`smi_during.sh` (`run: omn_launches_tool`) and both observe **the same
run**. nsys and ncu inject into the CUDA process through the hook omn's
CUPTI library uses (`CUDA_INJECTION64_PATH`), so they cannot share a process
with `omn`: those pairs are **separate runs** (`run: separate`), and they
compare what the workload decides -- counts, held exactly -- and durations,
medians of three runs a side, with the spread between runs as the band.
`gpu.mem` is a separate pair too, for a gap of omn's (gap 1 in
`docs/fixes/w22-gpu.md`).

**The runner's verdict, 2026-10-04: 21 skipped**, every pair on
`` `/dev/nvidiactl` is not installed here ``: the machine had rebooted into a
kernel the NVIDIA module was not built for, so no GPU work could run
(`docs/fixes/w23-gpu.md`). No pair has yet held under the shared runner,
and none counts as held until the four `omn_*.json` captures the `omn`
parsers are tested on are recaptured on a loaded driver: they are composed
by hand in today's `omn` shape, not output `omn` produced (the commands are
in the handoff's "Privileged runs wanted").
The numbers in the table are the twenty-second wave's, measured by the
family's own harness (since deleted) on the same pairs: two full runs, 19
of 21 then 20 of 21, the misses changed as `docs/fixes/w22-gpu.md` says.
They are the expectation the runner's first run on a loaded driver is held
to, not its verdict.

| Tool and use | omn equivalent | Covered | Same numbers | Missing |
|---|---|---|---|---|
| `nsys stats -r cuda_gpu_kern_sum`: launches per kernel | `omn trace gpu.kernel -e topk:20@kernel -- prog` (`events`) | every kernel of a launched program | runner: skipped here (no driver). Wave 22: exact, 5000/5000 launch-bound, 5/5 memory-bound, no record dropped (`kernel-launches-nsys`) | a `--pid` target (CUPTI loads at `cuInit`; nsys cannot attach either) |
| `nsys stats -r cuda_gpu_kern_sum`: total device time per kernel | same (`total`) | the total | runner: skipped here. Wave 22: within 0.3 % launch-bound in three runs (6 199 931 against 6 217 559 ns), within 1 % memory-bound in five runs of six (309 918 against 309 600); the sixth 7.1 % apart under load (`kernel-time-launch-bound-nsys`, `rel 0.05`; `kernel-time-memory-bound-nsys`, `rel 0.10`) | nsys's Avg, Med, Min, Max, StdDev per kernel: `topk` keys by an attribute, `summary` and `hist` do not (`summary@kernel` is refused), so a per-kernel distribution needs `-e raw` and a script (gap 2) |
| `nsys stats -r cuda_gpu_mem_time_sum`: copies and memsets, count and time | same, `kernel=[memcpy HtoD]` and the like | HtoD, DtoH, DtoD, memset | runner: skipped here. Wave 22: count exact (`copy-count-nsys`); time within 25 %, the pageable copy's own spread between runs of one tool (1.32 to 1.56 ms under nsys) (`copy-time-nsys`) | the bytes per copy (nsys's `cuda_gpu_mem_size_sum`) are not compared |
| `nsys stats -r cuda_api_sum`: driver calls, count | `omn trace gpu.api -e topk:20@api -- prog` | launch, copy, synchronize, allocation | runner: skipped here. Wave 22: exact, `cuLaunchKernel` 5/5, `cuMemAlloc` 3/3, `cuMemcpyDtoH` 1/1, on a program linked with `-lcuda` (`api-launches-nsys`, `api-allocs-nsys`, `api-copies-nsys`) | calls nsys lists and omn does not interpose: `cuMemsetD8`, `cuModuleLoadData`, `cuCtxCreate` (gap 3); a program reaching the driver through `dlsym` or `cuGetProcAddress` (ctypes, the CUDA runtime) shows nothing under `gpu.api` and everything under nsys (gap 4) |
| `nsys stats -r cuda_api_sum`: host time per call | same (`total`) | yes | no pair: the same 16 MiB synchronous copy took 1.46 and 3.47 ms of host time in two nsys runs, 1.52 and 1.58 under omn. nsys's tracing sits inside the call it times, so it is no reference for that duration | -- |
| `nsys profile` timeline (`cuda_gpu_trace`, the GUI) | `omn trace gpu.kernel -e timeline`, `omn view` lanes per stream | kernels, copies and memsets with device times on `CLOCK_MONOTONIC` | no pair: a timeline is the records the nsys pairs above compare | NVTX ranges (no source reads them, gap 5); nsys's OS runtime and CPU sampling rows are other families' (`sched.*`, `cpu.profile`) |
| `ncu --metrics smsp__inst_executed.sum` | `omn trace gpu.counters -e raw -- prog` (`value`, the median over launches) | the five default counters, others through `OMN_CUPTI_METRICS` | runner: skipped here. Wave 22: exact, 808 192 (copy) and 353 620 672 (spin) under both (`inst-copy-ncu`, `inst-spin-ncu`) | the value of each launch (ncu prints a row a launch; omn the median beside `min` and `max`) |
| `ncu --metrics sm__ctas_launched.sum`, `l1tex__t_bytes.sum` | same | yes | runner: skipped here. Wave 22: exact, 272/272 blocks, 33 554 432/33 554 432 bytes (`blocks-ncu`, `l1-bytes-ncu`) | -- |
| `ncu --metrics dram__bytes_read.sum` | same | yes | runner: skipped here. Wave 22: within 0.4 %, medians on both sides: 16 839 584 against 16 782 752 (`dram-bytes-ncu`, `rel 0.05`) | -- |
| `ncu --set full` and its sections (launch statistics, occupancy, roofline, source counters) | `gpu.counters` with `OMN_CUPTI_METRICS`; `gpu.profile` for where in the source | the raw metrics a section is computed from, named one by one | no pair: a section is ncu's derivation, not a counter | the launch's own facts -- grid, block, registers, shared memory, theoretical occupancy -- as values (gap 6); ncu's per-line instruction counts are not what `gpu.profile` samples (warp stalls by PC), so neither is a reference for the other |
| `nvidia-smi --query-gpu=power.draw -lms N` | `omn trace gpu.power -m count -e rate -- prog` (W) | the board's draw over a launched program | runner: skipped here. Wave 22: within 0.5-5.6 %, same run: 223.0 against 224.1 W, 200.5 against 203.3, 197.6 against 204.7, 216.2 against 229.0 (`power-smi`, `rel 0.10`) | -- |
| `nvidia-smi dmon -s p` (`pwr`) | same | yes | runner: skipped here. Wave 22: within 0.3-2.3 %, same run: 210.7 against 211.5 W, 211.1 against 216, 205.8 against 206.5 (`power-dmon`, `rel 0.15`) | -- |
| `nvidia-smi --query-gpu=clocks.sm`, `dmon -s c` (`pclk`) | `omn trace gpu.freq -m poll -e summary -- prog` (Hz) | the shader clock | runner: skipped here. Wave 22: within 0.4 %, same run: 1908 against 1908.75 MHz, 1905 against 1897.5 (`clock-smi`, `clock-dmon`, `rel 0.05`) | dmon's `mclk`: `gpu.freq` carries the memory clock as an attribute of each raw event (`memory_hz`), which `summary` does not aggregate (gap 7) |
| `nvidia-smi --query-gpu=utilization.gpu`, `dmon -s u` (`sm`) | `omn trace gpu.engine_busy -m poll -e summary -- prog` | the device-wide busy share | runner: skipped here. Wave 22: within 0.08 absolute, same run: 0.771/0.799, 0.851/0.867, 0.876/0.863, 0.744/0.818; dmon 0.761/0.761, 0.86/0.92, 0.882/0.905 (`busy-smi`, `abs 0.10`; `busy-dmon`, `abs 0.15`) | dmon's `mem` column (the memory controller's busy share) and `nvidia-smi pmon` (the busy share per process): `gpu.engine_busy` has no process key (gap 8) |
| `nvidia-smi --query-gpu=temperature.gpu`, `dmon` (`gtemp`) | `omn trace gpu.temp -m poll -e summary -- prog` (C) | yes | runner: skipped here. Wave 22: within 0.4 C, same run: 73 against 72.9 (`temp-smi`, `abs 4`) | -- |
| `nvidia-smi --query-compute-apps=pid,used_memory` | `omn trace gpu.mem -m poll -e summary -- prog` (`max`, bytes) | the VRAM a launched process holds | runner: skipped here. Wave 22: to the byte, 432 013 312 under both (412 MiB: 256 allocated on a 156 MiB context) (`vram-process-smi`, separate runs) | the launched program's descendants: under a wrapper omn reads the wrapper's pid and reports 0 (gap 1) |
| `nvidia-smi --query-gpu=memory.used` (device-wide) | `gpu.mem` with `--system` | yes | no pair yet: the NVML figure of `vram-process-smi`, device-wide | -- |
| `nvidia-smi dmon -s v` (violations), `-s t` (PCIe), `-s e` (ECC) | none | **no** | no pair: omn has no source for them | PCIe throughput, throttle reasons as values, ECC and replay counters: no `gpu.*` source reads them (gap 9). The clock event reasons are stated in every NVML run's notes, not measured |
| `nvidia-smi` (the table), `nvidia-smi -q` | the facts of every NVML run: persistence, clock lock, application clocks, clock event reasons; `omn top` for the processes | the state a measurement depends on | no pair: facts, not values; persistence and the max shader clock agree with `nvidia-smi` in the GPU scenarios (STATE.md) | -- |
| `nvidia-smi -lgc`, `-pm`, `-ac` (setting the device) | none, on purpose | -- | no pair: omn reads the clock state and never changes it | not a gap |
