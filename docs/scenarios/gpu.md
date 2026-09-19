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
`forcing_a_backend_this_build_does_not_implement_exits_three_and_names_it`
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
(`vendor:cupti`, `preload`, `vendor:ncu`) and says the request is valid. The
other two read `?`, which is a requirement no probe covers and is not the same
as `no`.

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
no backend on NVIDIA and the other three have no collector in this build.
`examples/gpu-job-latency.yml` is kept and kept running so that a machine with
`amdgpu`, `i915` or `xe` measures it; here the scenario is the refusal.

**`gpu.counters`, even once written.** Worth recording now because it changes
what a scenario for it can look like: it replays kernels, which destroys the
timeline, and the catalog already says only the counters a workload dominates
are reproducible. A scenario for it will have to choose its counters, not just
its workload.

---

## Defects found

Six, none worked around. Each is pinned by a test that asserts today's
behaviour, so fixing any one of them fails that test and brings whoever fixed it
here.

### 1. The one time error this family records reaches no output it can produce

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

Pinned by `the_one_time_error_this_family_records_reaches_no_output_it_can_produce`.

### 2. A `count`-mode run over-reports its own window by up to a second

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

Pinned by `a_count_mode_run_reports_a_window_longer_than_the_program_it_watched`.

### 3. `rate` of a ratio is a quantity of nothing

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

Pinned by `this_family_holds_the_catalogs_only_rate_over_a_ratio`.

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

### 6. A target that exited but was not reaped is followed to the end of the run

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

Pinned by `a_target_that_exits_without_being_reaped_is_followed_to_the_end`.

---

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
