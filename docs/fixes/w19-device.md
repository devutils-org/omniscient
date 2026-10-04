# The nineteenth wave — stream device

Owns `src/collect/gpu/{nvml,mod}.rs`, the `vendor:nvml` lines of
`catalog/gpu.yaml`, `tests/{family,scenario}_gpu.rs`, `docs/scenarios/gpu.md`,
and a section appended to `docs/families/gpu.md`. One file outside the row:
`src/collect/gpu/device.rs` (section 6).

Measured on the development machine: GeForce RTX 2080 Ti, driver 615.71.09,
while other streams used the board.

## 1. What landed

- **Clock state, read and never set.** `collect::gpu::ClockState` (serde,
  per device): the shader clock lock (`locked` / `unlocked` with its range /
  `unknown`), the application clocks (`set` / `default` / `not_supported` /
  `unreadable`), persistence mode, and the clock event reasons by name
  (`nvmlDeviceGetCurrentClocksEventReasons`, falling back to its older
  "throttle" name; unknown bits kept as `unnamed:0x..`). New NVML entry points
  in `nvml.rs`: `nvmlDeviceGetMaxClockInfo`, `nvmlDeviceGetClock` (app target
  vs default), `nvmlDeviceGetPersistenceMode`, the event reasons,
  `nvmlDeviceGetPerformanceModes` (v1 struct, versioned, bounded read).
- **Every NVML run states it** in its facts (`results[].notes`): one line per
  device at the start, and one at the end where the event reasons moved.
- **For GPU timings**: `collect::gpu::timing_clock_facts()` returns, per
  device, the state and -- unless a lock was *shown* -- "GPU timings on device
  N (...) were taken on unlocked clocks" (or "clocks not shown to be locked"
  when unknown). Unknown is never counted as locked. For cupti's and profiler's
  `facts()` (section 7).
- **Repeated readings are counted and said.** The driver refreshes the energy
  counter and the draw every 100 ms, and the occupancy about once a second.
  `gpu.power` and `gpu.engine_busy` events carry `driver_sample: new|repeat`.
  A run with repeats gets a degradation: "90 of 101 readings found the energy
  counter where the previous reading left it ... it moved 10 times in 997 ms,
  about every 99 ms and this run read it every 10 ms". Clocks, temperature and
  memory are not counted, because the same reading twice may just mean a
  steady value.
- **The default poll stays at 1 s.** That is already at or above every
  refresh measured. A faster explicit poll is honoured and its repeats are
  said, rather than silently re-paced.
- **NVML's cost per call kind**, measured in the thread's own cpu (the driver
  spins, so cpu ≈ wall), went into the caveats and `docs/scenarios/gpu.md`:
  persistence 6-9 µs, temperature 12-48 µs, memory ~85 µs, process list
  0.13-0.66 ms, occupancy 0.6-1.4 ms, event reasons 0.65-2.6 ms, one clock
  domain 0.8-2.1 ms, performance levels ~1.9 ms, energy 2.8-4.4 ms, draw
  2.8-7 ms. The old caveats' "34 ms a read" and "15 ms a read" were the
  bench's per-*run* figures.
- **Defect 7, found and said: reading the energy counter often makes it
  under-count**, for every reader of the board, across processes. Idle, with
  reads at the two ends only: 23 W. Read every 100 ms: 9-14 W. Every 10 ms:
  ~3 W, while the draw read beside it rose to 36 W (P8 to P5). A count window
  measured 22.5 W alone, 3.5 W while another process polled the counter every
  10 ms, and 23.4 W after it stopped. A `gpu.power` poll whose counter
  implies less than half the mean draw beside it now says so with both
  figures (`CounterCheck`). The caveat names `--mode count`. This also
  revises how `docs/families/gpu.md` read its 20 Hz counter÷integral gap.
- Tests: unit (`mod.rs`: parser on this board's text and on `nvml.h`'s
  example, lock verdict, reason names, timing line, state vs `nvidia-smi`;
  `device.rs`: repeat counter, counter check, fast-vs-slow separation,
  facts; `nvml.rs`: cost per call kind with a separation assert),
  `family_gpu::a_device_run_records_the_state_of_the_clocks_it_read_and_never_set`,
  `scenario_gpu::a_power_poll_faster_than_the_driver_refreshes_counts_its_repeats_and_says_so`.
  Device tests that read the energy counter now share a lock with the test
  that polls it fast.

## 2. Shown, and reasoned

Shown on this board: refresh rates (value changes polled every ms; the
driver's own sample timestamps for occupancy), per-call costs (three runs),
the counter under-count (several interleaved runs, plus the cross-process
run), persistence mode and the max shader clock agreeing with `nvidia-smi`,
and the repeat counts and notes through `omn`.

Reasoned, not shown:
- **That a `nvidia-smi -lgc` lock shows in the performance levels** as
  min = max at every level. NVML has no reader for locked clocks, and
  `nvidia-smi -q` prints none. I could not lock the clocks (forbidden). The
  error goes in the safe direction: a locked board read as unlocked gets an
  extra warning, and nothing is ever falsely claimed locked.
- That the 100 ms / ~1 s refresh holds on other boards. The notes report what
  each run observes, not these constants.
- The counter under-count's mechanism (it belongs to the driver). The numbers
  are noisy while other streams load the board, so the table uses quiet
  moments. The separation (10 ms vs ends-only) held in every run.

## 3. What I could not do, and why

- **The run's environment** (`capabilities.rs`, the coordinator's) does not
  yet carry the clock state. `ClockState` is ready for it (section 7).
- **The facts line of `gpu.kernel` / `gpu.profile`**: those are cupti's and
  profiler's files. The function is ready (section 7).
- **A per-source default poll** would need `request.rs`/the catalog loader.
  It is not needed today, because 1 s ≥ every refresh measured.
- Locking clocks or toggling persistence to verify the verdicts: privileged
  (section 8).

## 4. Decision lines for `DECISIONS.md`

- **The device's clocks are reported, never changed** (moves to settled):
  every NVML run states each device's shader clock lock, application clocks,
  persistence mode and clock event reasons before and after the run; a GPU
  timing whose lock was not shown says so. *Why*: locking needs root and does
  not survive a reboot; an unlocked clock makes a timing depend on what ran
  before it. A lock is claimed only from evidence (every performance level at
  one shader frequency, or application clocks set). Unknown is never locked.
- **A reading the driver had not refreshed is counted and said, not
  dropped and not re-paced**: `driver_sample: repeat` per event, a
  degradation per run with the observed refresh. Only figures whose repeat
  can be told from a steady value (energy counter, occupancy).
- **The default poll stays 1 s**, at or above every NVML refresh measured
  (100 ms energy and draw, ~1 s occupancy).
- **Energy is `--mode count`**: polling the counter starves it, for every
  reader of the board. A poll that sees its own counter below half its draw
  says so.

## 5. The suite

One `make check` on this tree: everything in my files green, red at
`tests/budget.rs::a_waiting_program_is_weighed_with_the_process_it_started_and_its_stop_ends_both`.
That test passed in the first of make check's two test passes and failed in
the second, under the load of the other streams. It is not GPU-related and
not in my files. Then one `cargo test --workspace --no-fail-fast`, with the
capabilities (`getcap`: `cap_perfmon,cap_bpf=ep`): **2 271 passed, 3 failed,
11 ignored**. The budget test passed this time. The three failures were
`collect::io::diskstats::tests::bytes_flushed_to_a_disk_show_up_in_its_counters`
and `unwind`'s `dwarf_recovers_the_whole_tree_of_a_build_without_frame_pointers`
and `fp_leaf_recovers_the_caller_of_a_frameless_leaf_and_marks_it`. All three
are outside this stream's files, and each passed when rerun alone once.
Every GPU test (unit, `family_gpu`, `scenario_gpu`) was green in both passes.

## 6. One file outside the row, and why

`src/collect/gpu/device.rs` is the NVML collector. No stream's row names it,
and the brief's bullets (repeats counted in the run, clock state in the run's
facts) can only live there. Additions only: `Refresh`, `CounterCheck`,
`facts()`, clock reads in `start`/`stop`, a `driver_sample` attribute, and
tests.

## 7. Needs from another stream

- **cupti** (`src/collect/gpu/kernel.rs`): in `gpu.kernel`'s collector,
  `fn facts(&self) -> Vec<String> { super::timing_clock_facts() }` (or read
  once at start and keep it: it opens NVML, 3-5 ms). This is what makes
  "every GPU timing carries its clock state" true for `gpu.kernel`.
- **profiler** (`counters.rs`, `profile.rs`): the same for `gpu.profile`.
  For `gpu.counters`, the state is worth stating too. The old catalog line
  "it locks clocks by default" (ncu's behaviour) no longer holds with CUPTI,
  and `omn` must not lock.
- **bench**: the `cost:` bands for `gpu.freq`/`gpu.power` can use the
  per-call table (`docs/scenarios/gpu.md`, "What can a poll get from the
  driver?"). The tool's cpu for a `gpu.power` poll is 6-11 ms per reading. A
  bench measuring GPU energy must not run beside anything polling NVML,
  this stream's fast-poll tests included.
- **coordinator** (`src/capabilities.rs`): `Environment.gpus` (or a sibling
  `gpu_clocks: Vec<collect::gpu::ClockState>`) filled from
  `collect::gpu::clock_states()` only when the request names a `gpu` source,
  because of `nvmlInit`'s cost (`docs/families/gpu.md`, "Still open: the GPU
  in the environment capture"). `render.rs` prints `ClockState::fact()`.
- **coordinator** (`docs/open-findings.md`): the "NVML's power query costs
  34 ms a read" finding is per run. Per read it is 6-11 ms. And defect 7 is a
  cross-cutting finding: any energy figure on this machine is at the mercy of
  another NVML poller.

## 8. Privileged runs wanted

Each run alone on the board, with nothing else using NVML. Restore after.

```console
# 1. Does a -lgc lock show in the performance levels, and does omn say "locked"?
sudo nvidia-smi -lgc 1500,1500
/home/moha/repos/omniscient/target/debug/omn gpu.temp --mode snapshot --emit summary -- true   # expect "shader clock locked at 1500 MHz"
python3 -c 'import ctypes as C;n=C.CDLL("libnvidia-ml.so.1");n.nvmlInit_v2();d=C.c_void_p();n.nvmlDeviceGetHandleByIndex_v2(0,C.byref(d))
class S(C.Structure):_fields_=[("v",C.c_uint),("s",C.c_char*2048)]
s=S();s.v=C.sizeof(S)|(1<<24);print(n.nvmlDeviceGetPerformanceModes(d,C.byref(s)),s.s)'
sudo nvidia-smi -rgc

# 2. Persistence mode read back as "on"
sudo nvidia-smi -pm 1
/home/moha/repos/omniscient/target/debug/omn gpu.temp --mode snapshot --emit summary -- true   # expect "persistence mode on"
sudo nvidia-smi -pm 0
```

If run 1 prints "not locked", the verdict needs another source of evidence.
File it against `mod.rs::shader_lock`.
