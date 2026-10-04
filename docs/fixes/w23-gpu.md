# Wave 23, stream gpu: handoff

Branch `feat/w23-gpu`, on `main` at `f42b3e8` (stream runner's schema and
stream cross merged). The gpu family's 21 pairs are the runner's now; none
of them could run on this machine this session, because the NVIDIA driver
was not loaded.

## The machine, this session

The development machine had rebooted (up 34 minutes at 21:38) into kernel
`7.2.8-arch1-2`, and `nvidia-open-dkms 615.71.09` has no module built for
it: `/usr/lib/modules/7.2.8-arch1-2` holds no `nvidia.ko`, `lsmod` shows only
`i2c_nvidia_gpu`, `/dev/nvidia*` does not exist, and `nvidia-smi` says "couldn't
communicate with the NVIDIA driver". Building or loading a module is not a
stream's to do (the brief: no module). So every GPU number in this handoff
is the twenty-second wave's, and every verdict the runner gave is a skip.

## What landed

- `catalog/references/gpu.yaml`: all 21 pairs in the README's schema, each
  with an `id` the table cites.
  - `same_run: alternate` became `run: separate`; the three duration pairs
    (`kernel-time-launch-bound-nsys`, `kernel-time-memory-bound-nsys`,
    `copy-time-nsys`) take `runs: 3` and compare medians, which is what the
    twenty-second wave's 7.1 % outlier on five launches asked for; the
    tolerances are unchanged.
  - `same_run: omn_launches_tool` became `run: omn_launches_tool` for the
    eight nvidia-smi pairs: `omn` launches `smi_during.sh`, whose samples
    the runner hands the parser from the tool's own stdout and stderr files.
    `--program-stderr` is gone.
  - `compare.omn_jq` is gone. The topk counts and totals are dotted paths,
    `results.0.outputs[emit=topk].series[key=kernel=tiny].events` (the
    filter splits on its first `=`, so a key holding `=` is spelt whole);
    the summaries and the rate `results.0.outputs[emit=summary].series.0.mean`
    and `...[emit=rate].series.0.per_second` (`.0`, not `.*`: `.*` would
    add two devices' series together on a machine with two GPUs).
  - Two `omn_parse` parsers where the grammar does not reach:
    `omn_topk` for `kernel=[memcpy HtoD]` (a `]` ends a filter) and
    `omn_counters` for `gpu.counters`' raw event picked by two attributes
    (`kernel` and `counter`) inside `attrs`. Both return the keys the
    tools' parsers give (`[memcpy HtoD]/count`, `copy/dram__bytes_read.sum`),
    so one `key` names both sides.
  - Every separate pair's `omn` has `-o {out}`; the workloads print nothing
    on stdout, but `linked.sh`'s program is not guaranteed to.
  - Every pair `requires` `/dev/nvidiactl` beside its tools; the ncu pairs
    `refused_when: [ERR_NVGPUCTRPERM]` (ncu's wording when performance
    counters are restricted to admin).
- `tests/reference/gpu.rs`: the six tool parsers unchanged; `omn_topk` and
  `omn_counters` added and registered; the family's own harness, the
  ignored `gpu_pairs_hold_on_this_machine` (jq, `same_run`, its own
  tolerance check), deleted -- nothing in the module runs a tool or `omn`.
  New tests: `omn_topk_reads_kernels_copies_and_calls_under_the_tools_keys`
  and `omn_counters_reads_each_kernels_counter_under_ncus_keys` (each also
  checks that every key the tool's parser gives on its capture is one
  `omn`'s parser gives), `the_omn_parsers_refuse_what_holds_no_value`, and
  `every_topk_path_finds_its_number_in_omns_output`, which loads
  `gpu.yaml` and evaluates each topk pair's own path with the runner's
  `eval_path` on the capture of its workload (6 paths).
- `tests/reference/captures/gpu/`: four `omn` outputs,
  `omn_kernel_topk.json`, `omn_kernel_topk_launches.json`,
  `omn_api_topk.json`, `omn_counters_raw.json`. **Composed, not captured**
  (the brief asks for captures of `omn`'s real output; the driver made
  that impossible): the `topk` and `raw` shapes are today's `omn`'s,
  captured from `cpu.instructions` on this build, the attributes are
  `src/collect/gpu/counters.rs`'s, the numbers the twenty-second wave's.
  The test module says so beside them.
- `docs/replaces/gpu.md`: every row cites its pairs by `id`; the "Same
  numbers" column says what the runner gave (skipped) and, separately, the
  twenty-second wave's harness numbers as the expectation, never as the
  runner's "same numbers".

## Shown vs reasoned

**Shown** (2026-10-04, this worktree, after `make`):

- `cargo test --test reference pairs_gpu -- --nocapture`: `tally: reference
  gpu: 0 held, 21 skipped, 0 gaps, 0 known defects, 0 failed`, every line
  `` skipped: reference gpu <id>: `/dev/nvidiactl` is not installed here ``.
  On CI that line matches `nvidia` in `.github/expected-skips.txt` (and
  `nvidia-smi` missing comes first in `requires` there).
- `cargo test --test reference gpu::`: 16 passed.
- `every_pair_is_well_formed` and `every_row_names_a_pair_or_why_none`:
  no gpu line in either (both are still red for other families' rows and
  pairs, which their streams own).
- Every `omn` argv of the pairs checked against today's `omn` with
  `--dump-request --no-resolve`: all eight resolve (exit 0), the modes and
  emits as written (`gpu.freq` etc. resolve `poll@1s`).
- The JSON shapes the paths read: `topk` series `{key: "comm=true",
  events, total}`, `summary` series `{key, count, min, max, mean, last}`,
  `rate` series `{key, events, total, per_second, ...}`, raw events'
  `attrs` as `[{name, value}]`: captured from `cpu.instructions` on this
  build.
- `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt
  --check`: clean.

**Reasoned, not shown**:

- That any pair holds under the runner. The expectation is the twenty-second
  wave's: 19 then 20 of 21 by the family's harness, the same commands and
  tolerances.
- That `run: omn_launches_tool` gives the nvidia-smi parsers what
  `--program-stderr` did. The parsers read stdout and stderr alike and keep
  only lines shaped as samples (tested on the mixed captures), and the
  tool's stdout and stderr are the wrapper's; so it should.
- The series' key of a device-wide NVML source (`.0` is read, whatever the
  key is): no NVML output could be produced.
- That `gpu.power`'s same-run pairs are not disturbed by nvidia-smi's 200 ms
  polling: `omn`'s own caveat says reading the board often makes its
  energy counter under-count for every reader. The twenty-second wave held
  0.4-5.6 % with that polling; the first run should watch for it.

## Each pair's outcome

All 21 **skipped**: `/dev/nvidiactl` is not installed here (driver not
loaded). `kernel-launches-nsys`, `kernel-time-launch-bound-nsys`,
`kernel-time-memory-bound-nsys`, `copy-count-nsys`, `copy-time-nsys`,
`api-launches-nsys`, `api-allocs-nsys`, `api-copies-nsys`, `inst-copy-ncu`,
`inst-spin-ncu`, `blocks-ncu`, `l1-bytes-ncu`, `dram-bytes-ncu`, `power-smi`,
`power-dmon`, `clock-smi`, `clock-dmon`, `busy-smi`, `busy-dmon`,
`temp-smi`, `vram-process-smi`. No gap, no known defect.

## Not done, and why

- Running the pairs: no driver (above).
- Real `omn` captures for `omn_topk` and `omn_counters`: same reason. Once
  the driver is loaded, replace the four composed JSON files by a real
  run's `results.json` (commands below) and move the tests' numbers to it.
- Step 3 (tool captures written from documentation): none was; the eight
  tool captures are the tools' real output from the twenty-second wave.

## Privileged runs wanted

1. Load the driver (the user's call: a module, root):
   `sudo dkms autoinstall -k 7.2.8-arch1-2 && sudo modprobe nvidia` (or
   reinstall `nvidia-open-dkms`, or reboot into a kernel it was built for).
2. Then, unprivileged, in this worktree, with nothing else on the GPU:
   ```console
   make
   cargo test --test reference pairs_gpu -- --nocapture
   target/debug/omn trace gpu.kernel -e topk:20@kernel --format json -o /tmp/k -- python3 tests/gpu_workload.py memory 5 16
   target/debug/omn trace gpu.kernel -e topk:20@kernel --format json -o /tmp/l -- python3 tests/gpu_workload.py launches 5000
   target/debug/omn trace gpu.api -e topk:20@api --format json -o /tmp/a -- tests/reference/workloads/gpu/linked.sh memory 5 16
   target/debug/omn trace gpu.counters -e raw --format json -o /tmp/c -- python3 tests/gpu_workload.py memory 5 16
   ```
   and copy each `results.json` over the matching
   `tests/reference/captures/gpu/omn_*.json`.

## Decision lines

- A pair whose machine has the tools but no loaded driver skips through
  `requires: [/dev/nvidiactl]`, not by failing in the workload: the runner
  runs the workload alone before granting an `omn` exit 1 as a skip, and a
  CUDA workload without a driver exits 2, which would fail every pair for a
  reason that is the machine's.
- A topk value is a dotted path with the series' whole key in the filter;
  a key holding `]` and an event picked by two attributes are parsers.
- Device-wide NVML series are read at `.0`, not summed over `.*`.

## Needs from another stream

- **coordinator**: the gpu verdict needs one run of `pairs_gpu` on a loaded
  driver, alone on the GPU (the pairs drive it for about two minutes now,
  with the three `runs: 3` pairs); until then `docs/replaces.md` should
  count gpu as not yet run by the runner.
- **runner** (for later, nothing blocks): a skip reason for a path in
  `requires` reads "is not installed here", which is right for a binary and
  odd for a device node (`/dev/nvidiactl`); "does not exist here" would read
  better for a path.

## Review

One finding, a gap: the `omn_topk` and `omn_counters` parsers and the topk
paths are tested only on four `omn` outputs composed by hand, not on output
`omn` produced; one sign of it, `spread` came after `min` and `max` in
`omn_counters_raw.json`, where `src/collect/gpu/counters.rs` inserts it at
index 4, before `max`.

- **Fixed**: the attribute order. `spread` is now at index 4 in all four
  events of `omn_counters_raw.json`, as `counters.rs` emits it (the parser
  looks attributes up by name, so no result moved). The test module's note
  said "three" composed captures; it says four, and that no gpu pair
  counts as held until they are recaptured.
- **Not fixable this round, for the same reason as before**: the recapture.
  The driver is still not loaded (21:49, up 44 minutes, no `/dev/nvidia*`,
  no `nvidia.ko` for `7.2.8-arch1-2`), and loading it is a root, module
  step that is the user's. The reviewer is right that the files must be
  recaptured before any gpu pair counts as held, and `docs/replaces/gpu.md`
  now says so beside the runner's verdict. The recapture commands are under
  "Privileged runs wanted" (step 2); after copying each `results.json`
  over its `omn_*.json`, the numbers in
  `omn_topk_reads_kernels_copies_and_calls_under_the_tools_keys` and
  `omn_counters_reads_each_kernels_counter_under_ncus_keys` move to the
  captured ones.
