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
  N (...) were taken on clocks not shown to be locked". A range in the
  performance levels is `ClockLock::NotShownLocked`, never "unlocked" (review,
  section 9). Unknown is never counted as locked. For cupti's and profiler's
  `facts()` (section 7).
- **Repeated readings are counted and said.** The driver refreshes the energy
  counter and the draw every 100 ms, and the occupancy about once a second.
  `gpu.power` and `gpu.engine_busy` events carry `driver_sample: new|repeat`.
  The counter's repeat is its value unchanged (it only rises); the
  occupancy's is the driver's newest sample timestamp unchanged
  (`nvmlDeviceGetSamples`), never its value (review, section 9).
  A run with repeats gets a degradation: "90 of 101 readings found the energy
  counter where the previous reading left it ... it moved 10 times in 997 ms,
  about every 99 ms and this run read it every 10 ms". Clocks, temperature and
  memory are not counted, because the same reading twice may just mean a
  steady value.
- **The default poll stays at 1 s.** That is at or above the counter's and
  the draw's 100 ms, and just under the occupancy's 1.0-1.13 s: an occasional
  occupancy reading at the default is a repeat, counted and said (1 of 9 in
  an 8 s run). A faster explicit poll is honoured and its repeats are said,
  rather than silently re-paced.
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
  `nvidia-smi -q` prints none. I could not lock the clocks (forbidden). So
  the verdict claims neither way without evidence: one frequency at every
  level reads "locked", a range reads "not shown locked" -- never "not
  locked", which on a `-lgc`-locked board would be a confident false
  statement.
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

## 4. Decision lines for `docs/DECISIONS.md`

- **The device's clocks are reported, never changed** (moves to settled):
  every NVML run states each device's shader clock lock, application clocks,
  persistence mode and clock event reasons before and after the run; a GPU
  timing whose lock was not shown says so. *Why*: locking needs root and does
  not survive a reboot; an unlocked clock makes a timing depend on what ran
  before it. A lock is claimed only from evidence (every performance level at
  one shader frequency, or application clocks set), and so is a free clock:
  a range in the levels reads "not shown locked", since a `-lgc` lock may not
  show there. Unknown is never locked.
- **A reading the driver had not refreshed is counted and said, not
  dropped and not re-paced**: `driver_sample: repeat` per event, a
  degradation per run with the observed refresh. Only figures whose repeat
  can be told from a steady value: the energy counter by its value, the
  occupancy by the driver's sample timestamp, never by its value.
- **The default poll stays 1 s**, at or above the energy counter's and the
  draw's 100 ms and just under the occupancy's 1.0-1.13 s; the occasional
  occupancy repeat at the default is counted and said, not avoided by a
  per-source cadence.
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

If run 1 prints "not shown locked", the levels do not show a `-lgc` lock and
the verdict needs another source of evidence: file it against
`mod.rs::shader_lock`. If it prints "locked at 1500 MHz", a range can then be
read as free: `shader_lock` may return a shown-unlocked verdict again, with the
run cited.

## 9. Review

One fix round, after an adversarial review. Each finding, and what came of it.

- **Defect: occupancy repeats counted by value** (`device.rs`). Right. An idle
  board at the default poll read 3 of 9 readings as stale and a 1600 ms
  refresh. Fixed: the occupancy's fingerprint is now the timestamp of the
  driver's newest sample (`Nvml::newest_occupancy_sample_us`, over
  `nvmlDeviceGetSamples`, asked only for samples newer than the last one
  seen). A driver with no sample buffer gets no occupancy count rather than a
  guess. The same command now reads 1 of 9, "about every 1143 ms". The reviewer
  was also right that the 1 s default is just under the occupancy's 1.0-1.13 s;
  the docs no longer say the default is at or above every refresh. The default
  stays (it is shared by every source, and the occasional repeat is said).
  The sample timestamp costs 0.7-2.4 ms of cpu per reading on top of the
  occupancy read (the caveat and the cost table say so). This is defect 8 in
  `docs/scenarios/gpu.md`. End-to-end test:
  `scenario_gpu::an_occupancy_repeat_is_a_driver_sample_seen_twice_not_a_value_seen_twice`
  (200 ms poll: most readings repeat; 1.5 s poll of an idle board: repeats
  claimed bounded by the gaps under 1.2 s in the run's own offsets, 0 here,
  while values repeat).
- **Gap: a range in the performance levels read as "unlocked"** (`mod.rs`).
  Right. `ClockLock::Unlocked` is now `ClockLock::NotShownLocked` (serde
  `not_shown_locked`); `locked()` answers `None` for it, the fact says "shader
  clock not shown locked (the driver's performance levels let it run from 300
  to 2205 MHz; a lock set with `nvidia-smi -lgc` is not known to show there)",
  and the timing line says "clocks not shown to be locked". Privileged run 1
  (section 8) decides whether a range can be read as free again.
- **Gap: the cost test reads the counter and the draw without the energy
  lock** (`nvml.rs`). Right. The lock moved to `collect::gpu::energy_counter_lock`
  (`mod.rs`, test-only), one for the whole `--lib` binary; the cost test and
  `device.rs`'s three counter tests all take it.
- **Gap: brief item 1 not delivered in this stream.** True, and not fixable
  from this row: the environment is `src/capabilities.rs` (coordinator), the
  timing facts are `kernel.rs` (cupti) and `counters.rs`/`profile.rs`
  (profiler). `timing_clock_facts()` is ready and tested; section 7 has the
  one-line patches. **Coordinator: the decision "a GPU timing taken on
  unlocked clocks says so in its facts line" stays unmet in this wave unless
  those land at merge.** Note the wording changed: the line now reads "taken
  on clocks not shown to be locked" on this board.
- **Nit: `device.rs` outside the row.** Disclosed in section 6; this round's
  fix also lives there, for the same reason.
- **Nit: "five levels"** in `docs/scenarios/gpu.md`. Wrong; the board's text
  has three (perf 0, 1 and 4). Fixed.
- **Nit: the slow power poll's note was checked only when it had no
  repeats.** Fixed: `note_agrees_with_events` holds every run's note to its
  events' marks, both ways, for the power test and the new occupancy test.

### The suite after the fix round

`cargo test --lib collect::gpu` (38), `scenario_gpu` (32) and `family_gpu`
(10) green, with the capabilities. One `make check`: red at
`cases::every_case_names_its_known_cause_both_ways` (`gcc-shell`'s on-cpu
391 ms from the reference under load), not GPU-related. Then one
`cargo test --workspace --no-fail-fast`: **2 288 passed, 1 failed, 11
ignored**; the cases test passed this time. The one failure was
`runner::a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed`,
which also failed alone. That is the known gap in `docs/open-findings.md`
("The one test that keeps `make check` red": per-ring throttles need
`src/perf.rs`), red on the wave's base, and not in this stream's files.
