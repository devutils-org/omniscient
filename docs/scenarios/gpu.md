# Scenarios — `gpu`

What is worth measuring on a GPU with this tool, and how you know the answer is
right.

The sessions are `examples/gpu-*.yml`, the checks are `tests/scenario_gpu.rs`,
and the workload they share is `tests/gpu_workload.py`. Everything below was
measured on the development machine: one NVIDIA GeForce RTX 2080 Ti, driver
615.71.09, 11264 MiB of VRAM, a 300 W board limit, under a Wayland compositor
that holds a context of its own.

---

## What this family is, before anything else

Nine sources, five with a collector, and **one mechanism behind all five**.

NVIDIA is proprietary: no DRM scheduler tracepoints, no exposed kernel PMU,
nothing under the driver to read. So every `gpu` source this build collects
goes through `libnvidia-ml.so`, opened at runtime, and the catalog says so by
naming the backend `vendor:nvml` rather than pretending there is a kernel
interface. AMD and Intel have the full stack — `gpu.job` reads their scheduler's
tracepoints, `gpu.engine_busy` their PMUs — and none of it is reachable here.

Three consequences shape every scenario in this document.

**Four of the five are device figures.** `gpu.freq`, `gpu.power`, `gpu.temp`
and `gpu.engine_busy` have no per-process meaning and are refused a `--pid`.
A launched program sets the window and nothing else, which the events say in
their `scope` attribute rather than leaving it to the reader's memory. Only
`gpu.mem` attributes anything to a process, because the driver keeps a
per-process VRAM figure and this reads it.

**The device is shared.** A compositor, a browser and a terminal all hold
contexts on the machine this was written on, and a device figure includes them.
Attributing a watt or a degree to the workload is not something this family can
do, and no scenario here claims it.

**The device has memory between runs.** It stays boosted, hot and drawing for
seconds after the program that made it so has gone, and NVML's occupancy figure
is an average over a window up to a second wide. Two runs taken back to back are
not two independent samples; this cost a whole afternoon and is measured under
"What cannot be verified here".

---

## Scenarios

### How much video memory did this program hold?

`examples/gpu-vram-step.yml` — **ground truth, exact**

The workload creates a CUDA context, holds it for 400 ms with nothing
allocated, allocates exactly **268435456 bytes** and holds that for 700 ms. The
number is in `tests/gpu_workload.py`, which is what makes it ground truth.

The check is on the **step**, not the level, because a CUDA context reserves
video memory before a single byte is asked for. One run's series:

```
bytes        device_used_bytes
0            726335488
0            726335488
10485760     736821248
23068672     749404160
163577856    889913344     <- the context, settled
...
432013312   1158348800     <- the context plus the allocation
...
0            726335488     <- the process is gone
```

`432013312 - 163577856 = 268435456`. Exactly, on every run of it so far, and
the device's own used figure — read from a different place in the driver —
moves by the same amount to the byte. The 156 MiB the context takes for itself
is in both plateaus and cancels.

Two tests:
`a_process_holds_exactly_the_video_memory_it_allocated` and
`the_process_figure_and_the_device_figure_move_by_the_same_step`.

### Did this program use the GPU at all?

`examples/gpu-vram-of-a-process.yml` — **ground truth, by construction**

A program that never opens a context. It holds nothing, and the run says
`bytes=0 held_as=none scope=process` on every read rather than returning an
empty series. "Nothing was measured" and "the measurement is zero" are different
claims about a run and a reader cannot tell them apart from silence. The same
event carries `device_used_bytes=726335488`, so the zero can never be misread as
a device that was empty — it was not, all of it was somebody else's.

Test: `a_program_that_never_touches_the_device_holds_zero_rather_than_nothing`.

### What did the device do while my program ran?

`examples/gpu-device-state.yml` against `examples/gpu-idle-baseline.yml` —
**a pair, and neither half means anything alone**

The same four sources, the same window, the same machine; a resident compute
kernel is the only difference. One pair, with the device settled first:

| | window | energy | draw over the second half | occupancy, first → last | clock | temperature |
|---|---|---|---|---|---|---|
| idle | 1.63 s | 35.7 J | 14 W | 0.00 → 0.01 | 405–1350 MHz | 46–48 °C |
| kernel | 2.01 s | 334.8 J | 184 W | 0.01 → 1.00 | 405–1965 MHz | 45–63 °C |

The workload's chosen property is that a kernel was executing when the busy
window closed and nothing was when the idle one did, so what is checked is the
**last** point of each occupancy series, not the maximum: the first points of
any run still carry whatever the device was doing before it started.

The power check is a direction — the kernel's draw must be at least twice the
baseline's — rather than the factor of thirteen in the table. The reason is
under "What cannot be verified here": the baseline is not a floor.

Test: `a_resident_kernel_separates_from_an_idle_device_on_occupancy_and_on_power`.
This is the slowest scenario in the family at **6.2 s**, most of it waiting for
the device to come down before the baseline is taken. It cannot be made quick;
the hardware sets the pace.

### Is the energy figure an energy figure?

`examples/gpu-energy-two-ways.yml` — **two roads, and they do not agree**

`gpu.power` reads one number and reports two. `joules` is the device's own
counter, rebased at the start of the run; `watts` is the instantaneous draw at
the moment of the read, riding beside the series. Integrating the second over
the window is a second estimate of the first. Three runs:

```
counter 221.1 J   integral 260.1 J   ratio 1.18
counter 304.4 J   integral 328.5 J   ratio 1.08
counter 309.6 J   integral 341.4 J   ratio 1.10
```

The sampled integral over-reads by eight to eighteen per cent, and did so on an
idle device too (about twenty per cent at a draw of 20 W). That is the catalog's
caveat with a number on it: integrating a 100 ms poll is not an energy
measurement. **When the two disagree, quote the counter** — it is the device's
own accounting and it does not miss what happens between two reads.

What the test asserts is the shape: the counter is monotone, both are positive,
and they stay within a factor of two. The ratio itself is a property of this
board and this poll interval, so it is recorded here rather than pinned there.

Test: `the_energy_counter_and_the_integral_of_the_sampled_draw_do_not_agree`.

### Does the clock reading mean anything?

**checked against the device's own limits and a second reader**

There is no workload property to check a clock against — see "What cannot be
verified here" — so this one is bounded instead. The shader clock must not
exceed what the device says its maximum is, the memory clock must not exceed
its own separate maximum (mixing the two domains into one series would show up
here as a memory clock above the shader ceiling), and this tool's reading must
fall inside the interval spanned by two `nvidia-smi` readings taken either side
of it.

Test: `the_reported_clock_stays_inside_what_the_device_says_it_can_do`.

### Were the clocks locked while I measured?

**read, never set; held to the vendor's tool where it has an answer**

An unlocked GPU runs its shader clock wherever the driver puts it, and where
that is depends on what ran before: a kernel timed a second after another one
is timed on a boosted, warm board. Locking the clocks needs root and does not
survive a reboot, so `omn` does not do it. Every run of an NVML source reads
each device's clock state before the observed program starts and again when the
run stops, and states it in the run's notes:

```
note: vendor:nvml: device 0 (NVIDIA GeForce RTX 2080 Ti): shader clock not shown locked
      (the driver's performance levels let it run from 300 to 2205 MHz; a lock set with
      `nvidia-smi -lgc` is not known to show there), no application clocks (this board
      does not have them), persistence mode off, clock event reasons: none
note: at the end of the run: vendor:nvml: device 0 (...): ..., clock event reasons: reliability
```

The end-of-run line appears only where the clock event reasons moved, which is
where a thermal or power limit reached during the run shows.

What each part rests on, on driver 615.71.09:

| part | read through | on this board |
|---|---|---|
| shader clock lock | `nvmlDeviceGetPerformanceModes`: the shader clock range of every performance level | three levels (perf 0, 1 and 4), 300 to 645 MHz at the lowest, 300 to 2205 at the highest: not shown locked |
| application clocks | `nvmlDeviceGetClock`, target against default | "not supported" -- `nvidia-smi` says "deprecated" for the same field |
| persistence mode | `nvmlDeviceGetPersistenceMode` | off, as `nvidia-smi` says |
| clock event reasons | `nvmlDeviceGetCurrentClocksEventReasons` | `gpu_idle` or `reliability` at idle |

**NVML has no reader for locked clocks.** `nvmlDeviceSetGpuLockedClocks` and
its reset exist; nothing returns what they set, and `nvidia-smi -q` prints no
such field either. The `applications_clocks_setting` event reason is documented
as "no longer used". So the verdict comes from the driver's performance levels,
and a lock is claimed only where *every* level runs the shader clock at one and
the same frequency -- an idle level pinned at its floor, which NVML's own
header shows at `perf=0`, is not a lock. That a `-lgc` lock shows in those
levels is **reasoned, not shown**: locking the clocks is a privileged run this
wave did not make (`docs/fixes/w19-device.md` has the command). So a range in
the levels is said as what it shows -- "not shown locked" -- and never as
"not locked": a board someone locked with `-lgc` would otherwise be told, with
confidence, that its clock was free. Nothing is read as locked without
evidence, and nothing as free without evidence either.

A GPU **timing** -- `gpu.kernel`, `gpu.profile` -- carries one more line for
each device whose lock was not shown, from `collect::gpu::timing_clock_facts`:
"GPU timings on device 0 (...) were taken on clocks not shown to be locked: the shader clock
ran wherever the driver put it, so a duration depends on what the device did
before it".

Tests: `a_device_run_records_the_state_of_the_clocks_it_read_and_never_set`
(`tests/family_gpu.rs`, persistence held to `nvidia-smi`),
`the_clock_state_agrees_with_the_vendors_own_tool` (the top of the shader range
held to `nvidia-smi`'s `clocks.max.sm`), and the parser and verdict unit tests
beside it, held to this board's text and to the example in `nvml.h`.

### What can a poll get from the driver?

**measured: the driver's own pace, and what each call costs**

NVML does not read the board when asked. The driver samples it at its own rate
and the library returns the last sample, so a poll faster than the driver reads
the same sample again: points on a timeline that look like measurements and are
one measurement repeated.

| figure | refreshed | how it was seen |
|---|---|---|
| energy counter | every 100 ms | 31 changes in 3 s polled every millisecond, median gap 100.4 ms, idle and under a resident kernel alike |
| instantaneous draw | every 100 ms | the same, 100.4 ms |
| occupancy | about once a second | the timestamps of the driver's own sample buffer (`nvmlDeviceGetSamples`), 1.0 to 1.13 s apart over 71 samples |
| temperature | not observable | a whole number of degrees: an unchanged reading may be a steady board |
| clocks, memory | at the call | they change when the board does |

So the run counts what it can tell. The energy counter only rises while the
board draws power, so a counter that has not moved is a sample read again. The
occupancy is not such a figure -- an idle board reads 0.02 from one sample to
the next, a saturated one 1.0 -- so its repeat is told by the timestamp of the
driver's newest sample (`nvmlDeviceGetSamples`, asked only for samples newer
than the one the previous reading saw), never by the value. Both are counted,
each event says `driver_sample: repeat` or `new`, and the run says how often
the figure actually moved:

```
degraded: vendor:nvml: 90 of 101 readings found the energy counter where the previous
          reading left it, so those points are one driver sample read again, not new
          measurements: it moved 10 times in 997 ms, about every 99 ms and this run read
          it every 10 ms; poll no faster than the driver refreshes it
```

A clock or a temperature that reads the same twice may simply be steady, and is
not counted: counting it would be counting a guess.

Counting the occupancy by its value was defect 8 (below): an idle board read
as a stale one.

**The default poll stays at one second.** It is at or above the 100 ms of the
counter and the draw. It is *not* at or above the occupancy's 1.0 to 1.13 s:
at the default, a reading now and then finds the sample the previous one read
(1 of 9 in the 8 s run measured after defect 8's fix), and the run counts and
says it. The default is shared by every source
(`Mode::DEFAULT_POLL_INTERVAL`), and a per-source one would need the request
layer; an occasional repeat, said, is not worth a cadence special to one
source.
A request that asks for faster gets what it asked for, with its repeats
counted and said, rather than a cadence quietly changed under it.

**What a call costs**, in the tool's own cpu time per call, three runs of
`what_each_kind_of_call_costs_the_calling_thread` while other streams used the
board. Thread cpu and wall time agree to within a few per cent: the driver
spins, it does not sleep, so every millisecond here is a millisecond of a core.

| call | cpu per call |
|---|---|
| persistence mode | 6 to 9 µs |
| temperature | 12 to 48 µs |
| memory (device totals) | 81 to 86 µs |
| process list (`gpu.mem` with a process) | 0.13 to 0.66 ms |
| occupancy | 0.6 to 1.4 ms |
| occupancy sample timestamp (read beside it) | 0.7 to 2.4 ms |
| clock event reasons | 0.65 to 2.6 ms |
| clock, one domain (`gpu.freq` reads three) | 0.8 to 2.1 ms |
| performance levels | 1.9 to 2.0 ms |
| energy counter | 2.8 to 4.4 ms |
| instantaneous draw | 2.8 to 7.0 ms |

A `gpu.power` reading is the last two: 6 to 11 ms, which at a 100 ms poll is a
tenth of a core. The 34 ms the bench reported for the same source is per run,
not per read. The clock state above is read twice a run, at its ends: the
performance levels, the event reasons, the application clocks and persistence
mode, a few milliseconds each time.

Tests: `a_power_poll_faster_than_the_driver_refreshes_counts_its_repeats_and_says_so`
(10 ms against 300 ms, as a separation, both runs' notes held to their events'
marks), `an_occupancy_repeat_is_a_driver_sample_seen_twice_not_a_value_seen_twice`
(200 ms against 1.5 s, the slow run's claim bounded by the gaps in its own
offsets), `a_poll_faster_than_the_driver_refreshes_repeats_and_a_slower_one_does_not`
beside the collector, and the cost test beside the library.

---

## The four classes

### A. The capability sweep

`every_mode_and_emit_a_collected_source_declares_is_served_or_refused_by_name`
walks the catalog rather than a list written in the test: `omn list gpu
--format json` gives the sources, their modes and their emits, and every pair of
the two is tried. Thirty pairs across the five collected sources. Each must be
either served — with an output whose `emit` is the one that was asked for — or
refused with exit 2 by a message naming **both** the mode and the emit, since
the pair is what the reader has to change.

Twenty-four are served. Six are refused, and correctly:

```
gpu.freq         snapshot    timeline   mode `snapshot` cannot produce `timeline`
gpu.power        snapshot    timeline   mode `snapshot` cannot produce `timeline`
gpu.temp         snapshot    timeline   mode `snapshot` cannot produce `timeline`
gpu.mem          snapshot    timeline   mode `snapshot` cannot produce `timeline`
gpu.engine_busy  count       timeline   mode `count` cannot produce `timeline`
gpu.engine_busy  poll        rate       mode `poll` cannot produce `rate`
```

The formats are swept the same way, in
`every_output_format_either_carries_an_emit_or_refuses_it_by_name`: every emit
of `gpu.temp` against all nine output formats. `text`, `json`, `yaml`, `toml`
and `ndjson` carry all three; `chrome` carries `raw` and `timeline` and refuses
`summary`; `speedscope` carries `timeline` only; `folded` and `pprof` carry
none of them. Every refusal names the format and the emit. The structured ones
are parsed, not merely counted as non-empty — a format that writes something
shaped wrong passes any test that only looks for output.

One inaccuracy, too small for the defect list: `omn formats` says "`chrome`
carries timelines", and it carries `raw` as well, as instant events. The
behaviour is right and the sentence is short by one word.

### B. The backend comparison

**This family has no two implemented backends, so there is nothing to compare.**
Twenty backends are declared across the nine sources and five are written, one
per collected source. Every alternative is for hardware this machine does not
have: `fs:sys:amdgpu`, `fs:sys:i915`, `pmu:i915`, `pmu:xe`, `pmu:amdgpu`.

So the comparison is with the refusal, which is what the brief asks for when a
chain has one implemented backend and others declared.
`forcing_a_backend_this_build_does_not_implement_names_it_in_the_chains_order`
pins every backend of every collected source in turn and requires:

- the pin is never silent — `backend ... is pinned, so the resolver's choice is
  bypassed; this is for the comparison bench, not for measuring` — whether the
  run then measures or refuses;
- an unimplemented one exits **3**, not 1 and not 2, and names the backend:
  `no collector is written for gpu.temp on backend fs:sys:amdgpu yet`;
- and the message says the request itself is valid, because a reader pinning
  `fs:sys:amdgpu` on a laptop needs to know whether they are missing hardware or
  missing code, and only one of the two is worth waiting for.

A backend in no chain at all is a third answer and gets exit 2:
`gpu.temp has no backend vendor:imaginary`
(`forcing_a_backend_that_is_in_no_chain_is_a_rejection_rather_than_a_gap`).

The nearest thing to a real comparison this family offers is the one inside
`gpu.mem`: the process's own VRAM and the device's used total come from two
different driver interfaces in the same read, and both move by exactly the
allocated 256 MiB. That is checked, above.

### C. The loss path

`examples/gpu-poll-too-fast.yml`.

There is no ring to overflow here and no kernel to throttle: every reading is a
library call, and the only thing that can go wrong with a library call in a loop
is that it takes longer than the loop's period. `gpu.mem` is the most expensive
of the five, because it walks the driver's process list on every read — a
quarter of a millisecond. Ask it for a reading every 200 microseconds:

```
too-fast  via vendor:nvml  (gpu.mem)
  1160 events over 315.92ms, stopped because the launched program exited with exit status 0
  LOSS: 0 dropped, 0 failed reads, 1159 late ticks
```

1159 of 1160 ticks late, counted, and leading the terminal output. Nothing was
dropped and no read failed — the distinction matters, because a run that
conflated them would send the reader looking for a driver problem that is not
there. The same source at 100 ms is late on almost none of them — the check is
that fewer than a quarter of the ticks were late, since a poll on a machine
running the rest of this suite in parallel is occasionally scheduled late and
the run counting that is the behaviour under test. It is the contrast that makes
this a measurement rather than a collector that always complains.

The second way a run of this family ends early is the target going away, which
is not a loss: `a_target_that_exits_before_the_duration_ends_the_run_and_names_it`
asks for ten seconds of a process that lives for one, and the run stops after
1.00 s saying `process 12345 exited`. A series that stops short and does not say
why looks like a collector that died. There is a hole in that check and it is in
the defect list below.

### D. The refusal

The three answers, kept apart.

**Exit 2, a rejection — the request cannot mean anything.** Seven of them in
`a_request_that_cannot_mean_anything_is_rejected_and_names_what_cannot`, each
checked for naming the thing that is actually wrong:

| request | message |
|---|---|
| `gpu.freq --pid 1` | `gpu.freq` cannot be measured per pid: the catalog declares no `pid` support for this source; hint: this source accepts launch, system |
| `gpu.temp --mode trace` | `gpu.temp` cannot be observed in `trace` mode |
| `gpu.temp --emit hist` | `gpu.temp` cannot emit `hist`; hint: this source emits timeline, raw, summary |
| `gpu.temp --mode snapshot --emit timeline` | mode `snapshot` cannot produce `timeline` |
| `gpu.freq --keys task` | `gpu.freq` does not carry the `task` key, so it cannot be joined on it |
| `gpu.temp --filter "kernel == 'x'"` | `gpu.temp` has no field `kernel` to filter on |
| `gpu.mem --iface lo` | `gpu.mem` is not a network interface source |

The `--pid` one is the one that matters most. A clock belongs to the device and
there is no per-process share of it; answering with the device's number under a
pid would be the worst bug this tool could have.

**Exit 3, no collector — this build does not implement it.** `gpu.kernel`,
`gpu.api` and `gpu.counters`. `gpu.kernel` reads `HERE = yes` in the listing —
`libcupti.so` loads on this machine — so nothing about the hardware is wrong and
the message has to say that: it names the backend it would have needed
(`vendor:cupti`, `preload`, and then `vendor:ncu`) and says the request is
valid. The other two read `?`, which is a requirement no probe covers and is
not the same as `no`.

*Since then* (the nineteenth wave): all three have collectors -- `gpu.kernel`
and `gpu.counters` through `vendor:cupti`, `gpu.api` through `preload` -- and
`vendor:ncu` left every chain: `ncu` replays a kernel from outside the process
and is an independent reader for the tests and the comparison, not a backend.
This section records what the refusal said when it was written.

**Exit 1, no viable backend — this machine cannot serve it.** `gpu.job`, and
this one is wrong: see the defect list. The *message* is right, and is checked:
it names the two DRM scheduler tracepoints that are absent rather than sending
the reader to a capability or a sysctl that would not have helped.

---

## Workloads

`tests/gpu_workload.py`, three commands. It reaches the device through
`libcuda.so.1` opened at runtime, so it needs no toolkit, no compiler and no
linked SDK, and a machine without the driver fails with a reason on standard
error and exit code 2. Nothing is printed on standard output, because a launched
program inherits the stream `omn` writes results to.

| command | what it guarantees |
|---|---|
| `arena <bytes> <settle_ms> <hold_ms>` | a CUDA context held with nothing allocated for `settle_ms`, then exactly `bytes` of device memory held for `hold_ms`. The step in a VRAM series is the allocation, to the byte. |
| `kernel <ms>` | a compute kernel resident on the device for the whole of `ms`, launches queued eight deep so the device never waits for the host between two of them. One launch occupies the device for about 20 ms here. |
| `idle <ms>` | no GPU is touched at all for `ms`. The baseline, and the only honest way to ask what a device figure reads with nothing on it. |

For phase two: `kernel` is the one worth reusing. It is a program that is
almost entirely *not* on the cpu — the host thread launches and synchronises —
so paired with `sched.offcpu` it is the inverse of the cpu-bound half that P2
needs, and `gpu.mem --keys task` is the only `gpu` series that carries a key to
join it on.

The PTX in the workload stores its accumulator at the end of the loop, and that
is not decoration: without the store `ptxas` deletes the loop, the kernel
returns instantly, and a "busy" workload leaves the device idle. That is how it
was written first, and the device read 20 % busy.

---

## What cannot be verified here

**The clock, against any workload.** A GPU that boosted a second ago is still
boosted. The fall back to idle takes about five seconds and happens in discrete
steps, so a clock reading taken inside any scenario's window says as much about
the previous scenario as about this one. The bounds and the second reader are
all this family can offer for `gpu.freq`.

**Any device figure, attributed to the workload.** `gpu.power`, `gpu.temp`,
`gpu.freq` and `gpu.engine_busy` are the whole device's, and on this machine a
display server, a compositor, a browser, a terminal and a password manager all
hold contexts — `nvidia-smi` lists six processes with nothing of ours running.
The idle baseline reads 14–22 W at rest, and the desktop's own work moves it:
two readings two seconds apart, with nothing of ours running, were 22.4 W and
50.0 W. A floor that wanders by a factor of two cannot support a ratio
assertion, which is why the busy-versus-idle power check is a factor of two when
the measurement is a factor of thirteen.

**The device's state between two runs.** This is the one that cost the most
time. The draw after a kernel comes down a staircase, and every tread is flat
enough to fool a "wait until it stops falling" rule:

```
 0.00 s   201.3 W      2.14 s    69.8 W      4.28 s    39.9 W
 0.31 s    99.4 W      2.44 s    67.5 W      4.59 s    29.4 W
 0.61 s    93.9 W      2.75 s    68.9 W      4.90 s    25.9 W
 0.91 s    94.6 W      3.05 s    68.1 W      5.21 s    25.4 W
 1.22 s    93.2 W      3.35 s    68.4 W      5.52 s    29.3 W
 1.53 s    94.6 W      3.66 s    67.7 W      6.15 s    22.6 W
 1.83 s    94.4 W      3.97 s    41.0 W      7.08 s    20.5 W
```

The first version of the test's settling rule waited for three readings within
a tenth of each other and returned 94 W, five times the floor. The rule is now
an absolute bar — fifteen per cent of the board's rated limit, which the vendor
tool states — and it takes about four seconds to clear.

**`gpu.engine_busy` as anything but a coarse yes or no.** NVML's utilization is
the fraction of a driver-chosen window during which at least one kernel was
resident. A kernel using one of 68 SMs and one using all of them both read
100 %. The window is up to a second wide and the driver refreshes the number
about once a second, so a 100 ms poll returns the same reading ten times over:

```
0.17 0.15 0.15 0.15 0.15 0.15 0.15 0.15 0.15 0.15 0.15 0.9 0.9 0.9 0.9 ... 1.0 1.0 1.0 1.0
```

Eleven identical points, then a step. The kernel started at about 400 ms into
that run and the figure moved at 1.1 s. The collector records the uncertainty
and no output can show it — that is the first defect below.

**`gpu.job`, `gpu.kernel`, `gpu.api` and `gpu.counters`, at all.** The first has
no backend on NVIDIA and the other three had no collector in this build when
this was written (they have since: `vendor:cupti` and `preload`, and
`vendor:ncu` is no longer in any chain).
`examples/gpu-job-latency.yml` is kept and kept running so that a machine with
`amdgpu`, `i915` or `xe` measures it; here the scenario is the refusal.

**`gpu.counters`, even once written.** Worth recording now because it changes
what a scenario for it can look like: it replays kernels, which destroys the
timeline, and the catalog already says only the counters a workload dominates
are reproducible. A scenario for it will have to choose its counters, not just
its workload.

---

## Defects found

Seven, none worked around. Each was pinned by a test that asserted today's
behaviour, so fixing any one of them failed that test and brought whoever fixed
it here.

Three now carry a marker, with the reproduction left in place: the session and
the output that found them are what says a regression has happened.
`docs/fixes/net.md` has what was done and what it cost.

### 1. The one time error this family records reaches no output it can produce — **half fixed**

*Declaring `raw` is stream B's and putting an error bar on a timeline point is
stream C's; both signatures are under "Needs from another stream" in*
`docs/open-findings.md`. *What the collector can do, and now does, is state the
error bar once — with its size — as a degradation of the run, which reaches
`text`, `json` and `chrome` whatever emit was asked for.* `ndjson` *carries
nothing of it, and that is a property of the format rather than of this source.
The test is renamed* `the_one_time_error_this_family_records_reaches_every_output_that_can_carry_it`
*and requires both the statement and the number: an uncertainty with no size is
not one.*

`gpu.engine_busy` is the only `gpu` source whose samples do not happen when they
are timestamped: NVML averages occupancy over a window up to a second wide, and
`src/collect/gpu/device.rs` correctly sets `ts_err_ns` to 1 000 000 000 on every
event. The source declares `timeline`, `rate` and `summary` — and **`raw` is the
only emit that serialises `ts_err_ns`**, which this source does not declare. It is
absent from the JSON timeline, from `ndjson`, and from the `chrome` encoder,
which does carry it — but only on the instant events it writes for `raw`.

```console
$ omn gpu.engine_busy --mode poll@200ms -e raw --system -d 500ms
error: [gpu.engine_busy #1] `gpu.engine_busy` cannot emit `raw`
  hint: this source emits timeline, rate, summary

$ omn gpu.engine_busy --mode poll@200ms -e timeline --format ndjson --system -d 600ms
{"key":"system","offset_ns":0,"record":"point","value":0.14}
{"key":"system","offset_ns":195680209,"record":"point","value":0.14}
{"key":"system","offset_ns":393671994,"record":"point","value":0.14}
{"key":"system","offset_ns":593634941,"record":"point","value":0.14}
```

Four points 200 ms apart, all one driver sample, and nothing in the output says
the timestamps are good to a second. The invariant in `CLAUDE.md` — timestamps
from elsewhere carry a non-zero `ts_err_ns` — is honoured in the event and lost
in the emit. Either the source declares `raw`, or a timeline point carries its
error bar.

Pinned by `the_one_time_error_this_family_records_reaches_every_output_that_can_carry_it`.

### 2. A `count`-mode run over-reports its own window by up to a second — **fixed**

*Fixed by stream F. The collector now takes the mode and, in `count`, splits two
things that were one: a ten-millisecond tick so the run notices its target
going, and reads throttled to* `Mode::DEFAULT_POLL_INTERVAL`, *which is about
where the driver refreshes them. Without the split, either the window stays
rounded up or the driver is asked a hundred times for one sample, and an NVML
read is milliseconds. The same tick for the same reason is in*
`collect/cpu/counters.rs`. *After: `sleep 0.3` reports 0.303 s and `sleep 1.5`
reports 1.510 s, with the event counts unchanged.
`a_count_mode_run_reports_the_window_of_the_program_it_watched` requires the
window to be the program's and the run to have stopped because its program did.*

*A first attempt refused `count` on this backend outright — NVML has no counter
to difference, and integrating a sampled ratio is the arithmetic this file
already refuses for energy — and was withdrawn: it would have quietly closed
defect 3, which is another stream's decision, and it would have added a ninth
instance of the pattern this family's own mode × emit sweep exists to keep out.
The reasoning is recorded as a need against* §1 *instead.*

`gpu.engine_busy` is the only `gpu` source declaring `count`, and no collector in
this family has a tick of its own in that mode, so the run falls back to
`Mode::DEFAULT_POLL_INTERVAL` — one second. The launched program's exit is only
noticed at a tick boundary, so the window is the program's life rounded **up** to
the next second:

```
sleep 0.3 : window 1.003 s, 2 events
sleep 1.5 : window 2.003 s, 3 events
```

The same program under `poll@100ms` reports 302 ms. This is not cosmetic: `rate`
divides by the window, and `rate` is reachable on this source *only* through
`count` mode, because `poll` cannot produce it. So the one number that uses the
window is computed with a window that is 3.3× too long for a 300 ms program.

Pinned by `a_count_mode_run_reports_the_window_of_the_program_it_watched`.

### 3. `rate` of a ratio is a quantity of nothing

**Fixed** by stream B of the fifth wave; see `docs/fixes/catalog.md`. The emit
is out of the entry, which now carries the caveat that says why, and the rule
is enforced rather than remembered: `src/validate.rs::check_catalog` refuses a
`rate` over any unit that does not add up — `ratio`, `hz` and `celsius` —
written as an exhaustive match so a unit added later has to answer the question.

```console
$ omn gpu.engine_busy --system --mode count --emit rate --duration 200ms
error: [gpu.engine_busy #1] `gpu.engine_busy` cannot emit `rate`
  hint: this source emits timeline, summary
```

What it used to answer:

`gpu.engine_busy` has unit `ratio` and declares `rate`, which sums the value
attribute and divides by the window:

```
  rate
    system 0.060 over 1.01s  =  0.060/s

  summary
    system n=2     min 0.030  mean 0.030  max 0.030  last 0.030
```

Two occupancy readings of 0.030 each, added together and divided by a second.
Occupancy is not additive: the sum of two of them is not a quantity, so neither
is a rate of it. The device was three per cent busy throughout, and `0.060/s` is
the only number offered for that.

`gpu.engine_busy` is the **only** source in the whole catalog that pairs unit
`ratio` with emit `rate` — every other source declaring `rate` is a `count`,
`bytes`, `ns` or `joules`, all of which add up. That makes it a catalog entry to
correct rather than a question about what `rate` means. The invariant is the one
in `CLAUDE.md`: an invalid combination is refused with the reason, never
degraded silently.

Pinned by `no_source_in_the_catalog_declares_a_rate_over_a_ratio`, which is the
same sweep over the whole catalog and now requires none rather than one. It was
`this_family_holds_the_catalogs_only_rate_over_a_ratio` while there was one.

One thing moved with it, and it is declared in `docs/fixes/catalog.md`: defect 2
above measured its over-long window through `--emit rate` on this source, which
no longer exists. It measures the same window through `--emit summary` — the
window is reported whatever the emit — and still fails if the window stops
being the program's life rounded up to the next tick.

### 4. A machine that cannot serve a valid request exits 2, like a bad request

> **Fixed** (stream A). Exit 1, from `exit_for` in `src/main.rs`.
> `a_source_this_machine_cannot_serve_names_the_tracepoints_that_are_absent`
> asserts the code; the reproduction below is unchanged.

The brief keeps three answers apart: a rejection is 2, no viable backend is 1,
no collector is 3. The middle one is not implemented. `src/main.rs` returns
`EXIT_REJECTED` for a `NoViableBackend` resolve failure, one line after printing
a hint that says the opposite:

```console
$ omn -f examples/gpu-job-latency.yml ; echo "exit=$?"
error: `gpu.job` has no viable backend on this machine:
  bpf:tp: tracepoint:gpu_scheduler:drm_run_job: missing: /sys/kernel/tracing/events/gpu_scheduler/drm_run_job does not exist; tracepoint:gpu_scheduler:drm_sched_process_job: missing: /sys/kernel/tracing/events/gpu_scheduler/drm_sched_process_job does not exist
  hint: the request is valid; it is this machine that cannot serve it. Use --no-resolve to write it down for one that can.
exit=2
```

The message is right and the exit code contradicts it. A script that wants to
skip what the machine cannot do, and stop on what the user got wrong, cannot
tell them apart — which is the whole reason the three codes exist. `gpu.job` is
the cleanest demonstration on this machine because it is the only source in the
catalog that reads `HERE = no` here, but the line is in the shared layer and
every family is affected.

This one also cost a test: the family sweep classified `gpu.job` as *collected*
because its probe did not come back with a 3.

Pinned by `a_source_this_machine_cannot_serve_names_the_tracepoints_that_are_absent`.

### 5. The loss report has one reason per late tick, not one reason

> **Fixed** (stream A) in `src/collect/mod.rs`, for every family at once. The
> reason is the kind, counted; the nanoseconds are in `losses.overrun` beside it.
> Asserted by `the_loss_report_carries_one_reason_for_all_the_late_ticks`.

`Losses::reasons` is documented as "the reasons behind the counts above, with
how often each occurred". The late-tick reason embeds the nanoseconds that tick
took, so every occurrence is its own reason and every count is one:

```
  LOSS: 0 dropped, 0 failed reads, 1095 late ticks
    1x a round of reads took 1920860ns, longer than the 200000ns tick
    1x a round of reads took 251844ns, longer than the 200000ns tick
    1x a round of reads took 253594ns, longer than the 200000ns tick
    ... 1021 more
```

1095 late ticks produced 1024 distinct reasons and 80 KB of JSON for a 300 ms
run. The count at the top is right; the explanation under it is a thousand-line
dump that buries the one fact it exists to carry. A single reason — "a round of
reads took longer than the 200000ns tick", counted 1095 times, with the worst
duration beside it — would say more in one line.

This is `src/collect/mod.rs` in the shared layer, not a `gpu` collector, and any
family that can be asked to poll faster than it can read will hit it.

Pinned by `the_loss_report_carries_one_reason_for_all_the_late_ticks`.

### 6. A target that exited but was not reaped is followed to the end of the run — **fixed**

*Fixed by stream F. The state letter in `/proc/<pid>/stat` is what tells a
zombie from a running process, and it is now what is read: `Z` and `X` are both
departed, and the run ends naming which. The reproduction below is what the tool
did before.* `a_target_that_exits_without_being_reaped_ends_the_run` *asserts the
run stops at the child rather than at the duration, and that what it reported is
the process while it existed rather than a tail of zeros for one that did not; a
unit test beside the collector waits for a child of its own to reach `Z` and
checks that its `/proc` entry is still there while the collector says it has
gone.*

`gpu.mem` ends a run when its process goes, and checks that by asking whether
`/proc/<pid>` exists. A zombie has one. A parent that has not yet reaped its
child therefore leaves the collector polling a process that no longer exists,
producing exactly the series of zeros the code's own comment says it is there to
prevent:

```console
$ # a child that exits after 400 ms and is deliberately not reaped
$ omn gpu.mem --pid 3677469 --mode poll@200ms --emit summary -d 2s
gpu.mem  via vendor:nvml  (gpu.mem)
  11 events over 2.00s, stopped because the requested duration elapsed
  no loss: every read succeeded and no tick ran late

  summary
    task:3677469 n=11    min 0 B  mean 0 B  max 0 B  last 0 B
```

Ten of those eleven readings are of a process that had already exited, reported
as a program holding no video memory. The state letter in `/proc/<pid>/stat` is
`Z` throughout and nothing looks at it.

This is not exotic: a shell reaps its background jobs, but a test harness, a
supervisor, or anything using `posix_spawn` and deferring the `wait` does not.
`a_target_that_exits_before_the_duration_ends_the_run_and_names_it` has to
orphan its target through a shell that exits, so that init reaps it, for the
positive case to be reachable at all.

Pinned by `a_target_that_exits_without_being_reaped_ends_the_run`.

### 7. Reading the energy counter often makes it under-count -- **said, not fixable here**

*The driver's, not this tool's: what can be done is to say it. A `gpu.power`
poll whose own counter falls below half the draw read beside it says so with
both figures, and the caveats name `--mode count`, which reads the counter
twice. Another program polling the board does the same to this run, and that
cannot be seen.*

Found by a unit test that went red only beside another: the check that the
counter's joules imply roughly the watts sampled across them failed, every
time, when the new test polling the counter every 5 ms ran beside it -- 31 W of
counter against 90 W of draw. Alone, it passed.

The counter's rate over a four-second window on the idle board, read only at
its two ends, against the same window with the counter and the draw read in
between (driver 615.71.09, RTX 2080 Ti, quiet moments):

| reads in the window | counter, W | draw read beside it, W |
|---|---|---|
| the two ends only | 22.5 to 24.0 | -- |
| every second | 20.7 to 23.4 | -- |
| every 250 to 300 ms | 18.5 to 19.7 | -- |
| every 100 ms | 8.8 to 14.4 | 24 to 29 |
| every 10 ms | 2.7 to 3.5 | 35 to 37 |

Three watts is less than the board can draw at all, and the draw went *up*
meanwhile: the board left its P8 idle state for P5 under the reads. The
counter is losing energy, and losing more the more often it is read. Reading
the temperature or a clock as fast does not do it; reading the counter or the
draw does, and either one alone is enough.

**It crosses processes.** A count-mode window -- two reads, four seconds
apart -- measured 22.5 W; the same window while another process read the
counter every 10 ms measured 3.5 W; 23.4 W again after it stopped. Every
`gpu.power` figure on this machine is therefore at the mercy of any other NVML
client polling the board: a monitor, a dashboard, another test.

Two things follow. The comparison in `docs/families/gpu.md` ("integrating
sampled power is not energy, and polling faster makes it worse") read a
counter÷integral of 0.64 to 0.75 at 20 Hz as the integral overstating; with this
measured, at least part of it is the counter under-counting under the 20 Hz
poll that sampled the draw. And this suite's own tests that hold the counter
against the draw now take a lock against the test that polls it fast.

Pinned by `a_counter_below_half_the_draw_beside_it_is_said_and_a_close_one_is_not`
and, on the board, by the scenario that polls at 10 ms and requires the note.

---

### 8. The occupancy's repeats were its values seen twice -- **fixed**

Found in review of the nineteenth wave. The first cut of the repeat counter
fingerprinted the occupancy pair by its bits: the guess the counter refuses
for temperature and clocks, because a steady value is not a stale one. On an
idle board at the default poll, `omn gpu.engine_busy --mode poll --emit
timeline -- sleep 8` read 0.02, 0.04, 0.02, 0.03, 0.05, 0.05, 0.02, 0.02,
0.02, called 3 of 9 readings one driver sample read again, put the refresh at
"about every 1600 ms" against the 1.0 to 1.13 s measured, marked new samples
`driver_sample: repeat`, and told the user to poll slower.

The fix takes the occupancy's identity from the driver: the timestamp of the
newest sample in its buffer (`nvmlDeviceGetSamples`, asked only for samples
newer than the one the previous reading saw). The same command now reads 1 of
9 as a repeat, "about every 1143 ms"; a 1.5 s poll of an idle board, whose
values repeat, claims none. A driver that keeps no samples gets no occupancy
count at all rather than a guess.

Test: `an_occupancy_repeat_is_a_driver_sample_seen_twice_not_a_value_seen_twice`.

## Cost of running these

The suite is 23 tests and takes about **12.3 s** wall, which is slow next to the
rest of the repository and is mostly the hardware rather than the tests.

They share a reader-writer lock over the device, and the split is worth
explaining because it is what keeps them honest. A test that reads a *number*
off the device — an occupancy, a draw, a clock — takes the lock exclusively:
one device serves the whole binary, a kernel left running by one test is the
next test's baseline, and even a sweep firing thirty requests at the driver
keeps the device awake enough to lift an idle baseline off its floor. A test
that reads the *shape* of an answer — an exit code, an emit name, a loss count
— shares, and several of those run at once.

| scenario | time | why |
|---|---|---|
| the busy/idle pair | 6.2 s | four seconds of it waiting for the device to reach its floor |
| the format sweep | 2.1 s | 27 runs |
| the mode/emit sweep | 1.8 s | 30 runs |
| the energy comparison | 1.8 s | a 1.2 s kernel window, plus the interpreter starting |
| the VRAM step, twice | 1.6 s each | 400 ms settle plus 700 ms hold, both chosen by the scenario |
| the target that exits | 1.1 s | the target lives for a second |
| the `count`-mode window | 1.1 s | the defect is that it takes a second; measuring it takes a second |
| the backend refusals | 0.9 s | thirteen pins, plus one probe per source |
| the unreaped target | 0.9 s | the run has to reach a duration it should not have reached |
| the loss path | 0.4 s | deliberately short; the point is the count, not the window |
| everything else | under 0.8 s | refusals, the listing, the catalog, the sessions |

Everything that needs a device skips with the reason the tool gave when there is
none, and everything that needs the workload skips with the reason `python3` or
`libcuda.so.1` gave. Both probes run once per suite. The sessions stay in
`examples/` either way, so a machine that can serve them runs them.

---

## What `gpu` contributes to a crossing

One line, for phase two: **`gpu` knows when the work left the cpu and did not
come back yet**, and `gpu.mem --keys task` is the only series in the family that
carries a key to join on — `task`, the process holding the video memory.

The useful crossing is `gpu.mem` against `mem.rss` and `sched.offcpu` for the
same task: a host thread that has launched a kernel is off cpu, and a program
whose resident memory and video memory move together is staging through the
host. Nothing else in this family can be joined, because nothing else in it
belongs to a process; `gpu.job` and `gpu.kernel` would both carry a `gpu` key
and both are out of reach here — the first for want of hardware, the second for
want of a collector.

---

## `gpu.job`, after the sixth wave

It has a collector now, and it still cannot be measured here. Those are two
separate facts and keeping them apart is the whole of what this section is
for — `docs/families/gpu.md` has the long version, including what was checked
and what remains a claim.

**What changed for a reader.** Before the wave, `omn gpu.job` on a machine
without a DRM scheduler was one silence covering two causes: no hardware, and
no collector. Now only the first is true here, and the exit code says so —
exit 1, the machine, not exit 3, the build. `omn list gpu.job` marks the
backend `collector written` rather than `no collector yet`. Both are asserted
in `tests/scenario_gpu.rs`, because a source that gains a collector and leaves
a *"not built"* message behind is the same lie as one that never had it,
pointing the other way.

**What the scenario can be, here.** Class D, the refusal, and nothing else.
`examples/gpu-job-latency.yml` is unchanged and still refuses; what was added
is a test that prints the machine's own numbers rather than describing them —
where tracefs is, whether its `gpu_scheduler` events are readable, and what is
under `/sys/class/drm`. On a machine that *can* read them it prints one thing
more: whether that kernel's `drm_sched_process_job` publishes the fence's
context and sequence number, which is the layout this build requires, or only
the `dma_fence` pointer, which it refuses.

**Why it refuses the pointer rather than using it.** A fence pointer is
recycled as soon as the fence is freed, so a completion keyed by one would be
attributed to whichever later job landed on the same address. It is the
argument the catalog already makes for scoping block requests by
`(ptr, open_ts)`, and the DRM scheduler has published the fence's identity
since Linux 6.11. A build that fell back to the pointer on an older kernel
would produce a latency distribution that looks exactly like a correct one.

**The one place this build bends the catalog.** The entry offers `ring` as a
filter field and the events carry the DRM scheduler kernel thread's name
instead of the tracepoint's own — the latter is a `__data_loc` string, and
reading one needs a variable-offset read of the record that this eBPF layer
does not have. On every driver with a DRM scheduler those threads are named
after the ring they serve, so the string is right for the wrong reason, and
every run says so in its own notes rather than leaving it here.
