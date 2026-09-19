# Scenarios — family `power`

What is worth measuring in this family, and how you know the answer is right.

Five sources, and they matter out of proportion to their size because they are
the confounder for every other family: a processor that throttled, or that ran
at one frequency under one instruction mix and another under the next, moves a
cpu, a memory and a gpu number at once for a reason that appears in none of
them. A run that throttled is not a slower run. It is a different run.

That makes the failure this family has to avoid a particular one. It is not
inaccuracy — it is a **quiet zero**: a machine that cannot see throttling
reporting none, a core whose residency is read from the wrong file, a package
whose energy is a plausible constant. Every scenario below is built to catch
that shape.

Read `BRIEF.md` for the four scenario classes, `docs/families/power.md` for
what the collectors had to decide, and `CATALOG.md` §7 for the vocabulary.

---

## The scenarios

| Question | Session | The property the workload chose | Checked by |
|---|---|---|---|
| Was this core busy, and busy at speed? | `power-core-under-load.yml` + `power-load-on-one-core.yml` | a spin loop pinned to core 1 never sleeps, so the core never halts | `the_frequency_of_a_busy_core_matches_the_cycles_that_core_issued`, `a_core_held_busy_never_enters_an_idle_state` |
| What did this window of work cost? | `power-energy.yml` | every core spinning is more silicon switching than none | `a_machine_with_every_core_busy_costs_more_than_an_idle_one` |
| How hot is the processor, and which sensor is that? | `power-every-axis.yml`, `power-confounders.yml` | hwmon's own reading of the same chip, read independently | `the_temperature_is_the_hottest_processor_sensor_hwmon_offers` |
| Does every axis the catalog declares work? | `power-every-axis.yml` | — (the catalog is the input) | `every_axis_this_family_declares_is_served_or_refused_by_name`, `a_result_survives_every_format_that_claims_it` |
| Does the tool say what it did not see? | `power-late-ticks.yml` | a 1 ms tick against a round of reads that takes 2 ms | `a_tick_shorter_than_a_round_of_reads_is_counted_and_surfaced` |
| Did this run throttle, and if we cannot tell, do we say so? | `power-throttle.yml` | — (the machine is the input) | `the_three_refusals_are_told_apart_by_what_they_say` |
| Do two roads to the same number agree? | `power-energy.yml` | RAPL read through perf against the same counter through powercap | `the_two_roads_to_rapl_agree_where_both_are_open` |

### Was this core busy, and was it busy at speed?

The strongest scenario here, because the check needs nothing from the tool but
the answer.

`taskset` pins a spin loop to core 1 from outside the tool. That loop never
blocks, so two things are true of core 1 for the whole window, and neither was
observed — both were arranged:

1. **It entered no idle state.** So `power.cstate --cpu 1` must report
   `idle_ns=0`, on every tick, and the assertion is an equality rather than a
   bound. Measured over a hundred consecutive ticks in one run: zero, every
   time.
2. **It never halted**, so the cycles the core issued over the window are its
   clock. `cpu.cycles` opened against the *core* counts exactly that, whoever
   was running on it, and the count over the window is the effective frequency
   by a route that shares nothing with `cpufreq`.

```console
$ omn -f examples/power-load-on-one-core.yml &
$ sleep 0.7 && omn -f examples/power-core-under-load.yml

  core frequency
    cpu:1 n=101   min 4.10 GHz  mean 4.18 GHz  max 4.25 GHz  last 4.18 GHz
  core idleness
    cpu:1 n=100   min 0ns  mean 0ns  max 0ns  last 0ns
  cycles the core issued
    cpu:1 4208134381 count over 1.01s  =  4179563649 count/s
```

4.1796 GHz from the cycle counter against 4.18 GHz from `cpuinfo_avg_freq` —
**the two agree to every digit the summary prints**, from a hardware counter
and from the processor's own aperf/mperf ratio. The test allows ten percent, which is loose on purpose —
what it is there to catch is a kHz left unconverted, a governor's requested
frequency substituted for the effective one, or the idle floor reported for a
boosting core, and each of those is a factor, not a percent.

Counting against the core rather than against the task is what makes this hold
on a shared machine. A task pinned to a contended core retires half the cycles
and would read as half the frequency; the core's own counter does not care how
many things ran on it. The residency check in the same session is what licenses
the division, which is why it is checked rather than assumed.

### What did a window of work cost?

Energy has no second instrument on this machine, so the check is a contrast the
scenario arranges: the same counted second with and without a spin loop on
every core.

```console
$ omn -f examples/power-energy.yml                      #  49.1 J/s
$ for i in $(seq $(nproc)); do sh -c 'while :; do :; done' & done
$ omn -f examples/power-energy.yml                      # 145.3 J/s
```

Sixty-four watts against a hundred and forty-five on an otherwise idle machine:
a margin no measurement error reaches. It is also the family's **scale** test.
RAPL counts in units of 2⁻³² joules and publishes what one tick is worth beside
the counter; a collector that assumed microjoules would be out by ten orders of
magnitude, so a package that lands in the tens of watts rather than the
nanowatts or the gigawatts is the evidence that the scale was read.

The test cannot assume it gets an idle machine — a suite that runs its own
tests in parallel does not offer one — so how busy the machine actually was is
read from `/proc/stat` over each window, independently of the tool, and the
margin demanded is tied to the change in busyness: `loaded > quiet × (1 + 0.25 ×
added)`. On an idle machine the measured coefficient is 1.27; a quarter of that
is a wide safety margin that still fails if the two windows cost the same. When
the machine was already saturated the contrast cannot be made at all, and the
test says so by name rather than passing on a coincidence.

### How hot is the processor, and which sensor is that?

There is no such thing as *the* temperature. This machine offers a package at
66 °C, an NVMe controller at 36 °C and a wifi chip at 53 °C, and their mean is a
number about nothing. The tool's job is a **choice**, so the check is against
that choice made independently: the test walks `/sys/class/hwmon` itself, keeps
the chips whose driver says they are processors (`k10temp`, `coretemp`,
`zenpower`, `cpu_thermal`, `k8temp`), takes the hottest, and holds the tool to
the driver it named and to the value in degrees.

```console
$ omn power.temp --system -m snapshot -e raw
  raw
    76012008553165  power.temp  celsius=69 sensor=k10temp/Tccd1
      basis=the hottest processor sensor, which is what throttling follows
      nvme.composite=36.85 k10temp.tctl=63 k10temp.tccd1=69
      k10temp.tccd2=66 iwlwifi_1.temp1=53
```

The tool's read is bracketed between two reads of hwmon taken around it, rather
than compared with one of them: a package under load moves several degrees a
second. The band is generous and what it still catches is everything
structural — a factor of a thousand left in millidegrees, or the NVMe
controller thirty degrees below.

### Does the tool say what it did not see?

Everything in this family polls a filesystem, so the way it loses data is not a
ring overflowing: it is a **tick it cannot keep**. A poll asked for a shorter
interval than one round of reads takes cannot deliver the series it promises,
and a series whose points are not where it says they are is a wrong number that
looks right.

So the scenario chooses the failure. `power.cstate` reads every idle state of
every core — twenty-four cores, three states, two files each — which is about
two milliseconds a round here. Asked for one millisecond it cannot keep up:

```console
$ omn -f examples/power-late-ticks.yml
  5688 events over 500.70ms, stopped because the requested duration elapsed
  LOSS: 0 dropped, 0 failed reads, 237 late ticks
    1x a round of reads took 1972261ns, longer than the 1000000ns tick
    ...
```

237 late ticks out of 237 rounds: **every** round, not a sample of them, which
is what the scenario arranged by choosing a tick shorter than a round. The same
session at `poll@200ms` reports no loss at all, and that contrast is the check —
a collector that reported loss unconditionally would pass half of it.

`dropped` stays at zero, correctly: nothing was dropped. What was lost is the
spacing, and calling it a dropped event would send the reader looking for a
buffer that is not there.

The budget is the other half of the same promise — a measurement cut short is a
window that does not cover what it claims to:

```console
$ omn power.cstate --system -m poll@1ms -e summary -b 1 -d 2s
  note: --budget 1%: `fs:sys` declares `free` (about zero, under 0.1 %), which is
    within it. What it actually cost is measured over the run and reported when it ends
  1128 events over 102.25ms, stopped because `power.cstate` spent 30.71 % of the
    target's cpu time against a ceiling of 1 %, and nothing cheaper than `fs:sys`
    is available
```

### Did this run throttle?

`power.throttle` exists to invalidate a run, not to describe one, which makes a
quiet zero here the most expensive lie the tool can tell: it reads exactly like
a clean run. On this AMD machine it must fail, and it does, naming two different
absences in two lines:

```console
$ omn -f examples/power-throttle.yml
error: no collector is written for `power.throttle` on backend `bpf:tp` yet
  `fs:sys` was passed over: /sys/devices/system/cpu/cpu0/thermal_throttle:
    missing: /sys/devices/system/cpu/cpu0/thermal_throttle does not exist
```

The sysfs route is an x86 thermal-interrupt interface this part does not
implement; the tracepoint route is there and nothing in this build attaches to
it. One is about the machine and one about the build, and a reader sent to buy
different hardware because of the second would have been misled. The session
ships so that an Intel machine with `thermal_throttle` measures with it.

---

## The four classes

| Class | Where it lives | What it found |
|---|---|---|
| **A** capability sweep | `every_axis_this_family_declares_is_served_or_refused_by_name`, `a_result_survives_every_format_that_claims_it`, `examples/power-every-axis.yml` | `hist` is declared by two sources and reachable through neither; `trace` on either is refused as a run failure rather than as a mechanism this build lacks; `snapshot` beside a poll is served as a poll |
| **B** backend comparison | `the_two_roads_to_rapl_agree_where_both_are_open`, `pinning_a_backend_this_build_does_not_have_names_it_and_exits_three`, `pinning_a_backend_this_machine_does_not_offer_names_what_is_missing` | the two `power.energy` backends disagree about what `count` means |
| **C** the loss path | `a_tick_shorter_than_a_round_of_reads_is_counted_and_surfaced`, `a_run_that_costs_more_than_its_budget_stops_and_says_which_ceiling`, `examples/power-late-ticks.yml` | nothing: the loss path is sound, counted per round and named in the text a person reads |
| **D** the refusal | `a_request_that_cannot_mean_anything_names_what_it_cannot_mean`, `the_three_refusals_are_told_apart_by_what_they_say`, `examples/power-throttle.yml` | the three refusals are two exit codes; the messages tell them apart and the codes do not |

The sweep is driven by the catalog rather than by a list in the test: every
mode and every emit each of the five entries declares is asked for in turn, and
each answer has to be one of four recognisable shapes — it measured, the request
cannot mean anything, this build has no collector, or a collector was chosen and
would not start. Each of the three refusals has to name the thing that is
actually missing. An answer of any other shape, or a success that measured
nothing, fails.

**Class B is thin here and that is the truth about the family, not a gap in the
scenarios.** Only `power.energy` has two written backends, and on an ordinary
machine only one of them is open: `energy_uj` has been root-only since the
counter was shown to leak what the machine was computing. So the comparison
skips, naming root, and what is checked instead is the half this machine can
answer — that forcing the closed road asks for privileges and does not blame the
hardware. The other four sources have one written backend each and three
declared-and-unwritten ones, and for those the comparison is with the refusal:
pinning an unwritten backend exits 3 and names it.

---

## Workloads

Two, both reusable, both with a property that is in their source rather than in
a measurement.

**A spin loop pinned to one core.** `taskset -c N /bin/sh -c 'while :; do :;
done'`, started from outside the tool, shipped as the target of
`examples/power-load-on-one-core.yml`. What it guarantees:

- core N is occupied for the whole window and enters **no** idle state, so its
  residency is exactly zero and its cycle count is its clock;
- it never blocks, so it is never woken onto another core and cannot migrate —
  `tests/family_sched.rs` relies on the same property for migration counts;
- every other core is left alone, so the same run carries its own control.

**Every core spinning.** `nproc` unpinned loops. What it guarantees is coarse
and physical: the package draws what a fully switching package draws. Measured
here, 145 W against 64 W idle. It is the only workload in this family that
moves the energy number far enough to be unmistakable, and it is what P4 should
reuse to ask whether frequency licensing explains a slowdown.

Both are cheap, both start in under a hundred milliseconds, and both are killed
by `Drop` in the test rather than by a signal at the end of the file.

---

## What this family contributes to a crossing

One line, as `BRIEF.md` asks:

> `power` knows what the machine was doing *to* the measurement — the clock the
> core actually ran at, whether it ever went idle, how hot it was and whether it
> was limited — and `cpu` carries none of it. The key is **`cpu`**.

`power.freq`, `power.cstate` and `power.throttle` all carry `keys: [cpu]`, and
every session here that can carry it does. `power.energy` and `power.temp` carry
none, and cannot: RAPL counts a package and there is no key for one, and a
temperature belongs to a sensor. Until a `package` key exists, those two join to
a run by being taken over the same window, not by a key — which is what
`examples/power-confounders.yml` is shaped for.

For P4 specifically — *does the machine's own state explain the measurement?* —
the crossing to build is `power.freq` against `cpu.instructions` by `cpu`. The
scenario above already does half of it in one session, against `cpu.cycles`, and
found the two instruments agree to every printed digit on a core that never
halted. The remaining half is the interesting one: a core under an
AVX-512 or heavy-vector mix carries a frequency licence, so the same
instruction count takes longer for a reason that is in `power.freq` and in
nothing the `cpu` family reports.

---

## What cannot be verified here

Stated rather than hidden, and each with what it would take.

**That the throttle counts are right.** Making a package throttle on demand
means holding it at its thermal limit, which is not something a test suite may
do to a machine it does not own. The refusal is verified; the arithmetic behind
the count is not, on any machine. What would settle it is a part with a
settable power limit — `intel-rapl` exposes `constraint_0_power_limit_uw` — but
writing it needs root and changes the machine for every other run on it.

**The `fs:sys` road into `power.energy`.** Root-only here, so the backend
comparison cannot be made. The test names root and the session stays in
`examples/`. Everything said below under defect 1 about that backend is read
from `src/collect/power/energy.rs`, not measured.

**Whether the frequency reported is the effective one on a machine that is not
this one.** `power.freq` chooses its file at startup — `cpuinfo_avg_freq`, then
`cpuinfo_cur_freq`, then `scaling_cur_freq` — and only the first two are
effective. This machine has `amd_pstate` and `cpuinfo_avg_freq`, so the cycle
oracle validates the effective path. On a machine that falls back to
`scaling_cur_freq` the same scenario would compare the governor's *request*
against the cycles actually issued, and they are allowed to differ — which is
the whole point of the source. The scenario would then be checking the wrong
thing, and a machine in that position should read `measures=requested` on every
event and not run the comparison. **Nothing in the tool or in the test refuses
it today**, and that is the one piece of this family's ground truth that does
not travel.

**Absolute temperature.** The check is that the tool picks the same chip hwmon
would and reports degrees rather than millidegrees. Whether the sensor itself is
accurate is a question about the part.

**`power.cstate`'s `state` filter field.** The catalog declares it and the
sysfs backend cannot carry it: one event covers every idle state of a core. The
tool is honest about it, which is why this is a limitation and not a defect:

```console
$ omn power.cstate --system -m poll@200ms -e raw -d 400ms --filter "state == 'c2'"
  0 events over 402.69ms, stopped because the requested duration elapsed
  filter `state == 'c2'` kept 0 of 48 events, evaluated after reading
    no event carried `state`, so that part of the predicate was never true
```

The residency of each state is on the event as `state.c1_ns`, `state.c2_ns` and
so on; what does not exist is a series per state, and the eBPF backend — which
sees transitions one at a time — is where that becomes real.

---

## Defects found

Four, none worked around. Each is reproduced below with the session that shows
it and the output it produced.

### 1. `power.energy -m count` is a series plus a zero, and the two backends disagree about what `count` means

`count` should be one total for the window. Through the `pmu` backend — the one
the resolver prefers, because it is the one that works without root — it is not:

```console
$ omn power.energy --system -m count -e summary -d 3s
  4 events over 3.00s, stopped because the requested duration elapsed
  summary
    system n=4     min 0.000 uJ  mean 51.11 J  max 92.54 J  last 0.000 uJ

$ omn power.energy --system -m count -e rate -d 3s
    system 153.98 J over 3.00s  =  51.32 J/s
```

Three seconds of a package drawing about 51 W is 154 J, which `rate` reports
correctly. `summary` on the same window says the smallest interval was zero, the
last was zero, and the mean was 51 J — the window's energy divided by one event
more than there were intervals.

Two causes, and they compound:

- `EnergyPmu::sample` emits a per-interval delta whether or not the mode is
  `count`, so a counted window becomes a series at the default poll interval.
  `EnergySysfs::sample` returns nothing in `count` mode and `EnergySysfs::stop`
  emits one event, the whole-window delta from a baseline taken in `start` —
  which is the intended shape. **The same request means different things
  through the two backends of one chain**, which is exactly what the backend
  comparison exists to find.
- `EnergyPmu::stop` then takes a further closing read. The runner's own
  convention is *read first, then decide*, documented in `drive()` so that "a
  polled counter's series [is] complete without any collector reading again in
  `stop`", and `the_runner_closes_the_window` measures how little time such a
  read could cover. Here it covers microseconds and lands in the series as a
  zero.

Consequences a reader meets: `examples/power-energy.yml` used to recommend
`-e summary` and would have shown the wrong number; it now uses `rate` and says
why. And a counted window rounds up to the next default poll interval, so
`-d 600ms` observes 1.0002 s.

Not fixed here: `src/` belongs to no scenario stream.

### 2. The three refusals are two exit codes

> **Fixed** (stream A), both divergences. Row two -- `power.throttle` without
> `CAP_BPF` -- exits 1; row four -- `power.freq -m trace` -- exits 3, because the
> refusal comes from the factory and nothing about this machine is involved in
> it. The man page is the document that now disagrees, and the replacement text
> is in `docs/open-findings.md`. Asserted by
> `every_axis_this_family_declares_is_served_or_refused_by_name`, whose exit-3 arm
> now accepts both ways of reaching that code.

`BRIEF.md` class D asks for three answers to be told apart, because they lead to
three different next steps — change the request, change the machine, write the
collector. Measured:

| Asked | Said | Exit | Brief asks for |
|---|---|---|---|
| `power.energy --pid 1 -m poll -e timeline` | the request cannot mean anything | 2 | 2 |
| `power.throttle` under a binary without `CAP_BPF` | `has no viable backend on this machine` | **2** | 1 |
| `power.throttle` with capabilities | `no collector is written for ... on backend bpf:tp yet` | 3 | 3 |
| `power.freq -m trace -e raw` | `cannot start collecting: a frequency change is an event, and events come from the power:cpu_frequency tracepoint` | **1** | 3 |

The middle two are the divergences.

The second row is a valid request this machine cannot serve, and it exits 2 —
the same code as a request that cannot mean anything. The message itself draws
the distinction the exit code then collapses:

```console
$ ./omn-without-capabilities power.throttle --system -m poll@200ms -e summary -d 200ms
error: `power.throttle` has no viable backend on this machine:
  bpf:tp: bpf: missing: loading a tracing program needs CAP_BPF and CAP_PERFMON ...
  fs:sys: /sys/devices/system/cpu/cpu0/thermal_throttle: missing: does not exist
  hint: the request is valid; it is this machine that cannot serve it.
        Use --no-resolve to write it down for one that can.
```

`man/omn.1` documents exit 2 as covering both, so the man page and the brief
disagree; whichever wins, a script cannot tell the two apart today.

The fourth row is the one class A was written to catch — *a mode that resolves
and then refuses at attach time*. `trace` is declared by `power.freq` and
`power.cstate`; the only backend that could serve it is `bpf:tp`, which has no
collector, so the resolver falls through to `fs:sys`, which cannot trace
anything. The message is exactly right and the exit code says "a failure while
running" rather than "this build does not implement it". `STATE.md` already
carries the neighbouring item — *an exit code for "valid, but this mechanism is
not built"* — and this is the same hole seen from the mode axis rather than the
backend axis.

The tests assert what the messages must say, not what the codes are, so nothing
here cements the current behaviour.

### 3. A `snapshot` request beside a polled one is served as a poll

> **Fixed** (stream A), both halves. A snapshot is a property of the request, so
> one written beside a poll is still a single read; and an interval of zero is
> "as fast as this can be read", which is not a cadence and cannot be fallen
> behind, so it no longer manufactures a late tick per round. The 158 losses that
> had not happened are gone with it. Asserted by
> `a_snapshot_beside_a_poll_is_one_read_and_invents_no_losses`.

`snapshot` means "a single read, after which the run is over". Alone, or beside
another snapshot, it is:

```console
$ cat snapshot-alone.yml
name: snap only
requests:
  - {source: power.temp, target: system, mode: snapshot, emit: [summary]}

$ omn -f snapshot-alone.yml
  1 events over 5.44ms, stopped because snapshot: a single read
```

Beside a poll, it is not:

```console
$ cat mixed.yml
name: mix
duration: 600ms
requests:
  - {label: a, source: power.temp, target: system, mode: poll@200ms, emit: [summary]}
  - {label: b, source: power.temp, target: system, mode: snapshot, emit: [summary]}

$ omn -f mixed.yml
a  via fs:sys  (power.temp)
  159 events over 602.16ms, stopped because the requested duration elapsed
  LOSS: 0 dropped, 0 failed reads, 158 late ticks
    2x a round of reads took 3738759ns, longer than the 0ns tick
  summary
    system n=159   min 63 celsius  mean 64 celsius  max 66 celsius  last 65 celsius

b  via fs:sys  (power.temp)
  159 events over 602.16ms, ...
  summary
    system n=159   min 63 celsius  mean 64 celsius  max 66 celsius  last 65 celsius
```

Request `b` asked for one read and got a hundred and fifty-nine. Two things are
behind it: the run is a snapshot only when **every** request is one
(`collect/mod.rs`, `let snapshot = resolved.iter().all(...)`), and
`power.temp`'s collector reports `tick() == Some(ZERO)` in snapshot mode, so the
window's interval — the minimum over the collectors — collapses to zero. Every
round is then counted as late against a "0ns tick", so the run also reports 158
losses that did not happen, which is the one thing this project promises never
to do in the other direction.

**This is not a `power` defect and the coordinator should read it as a shared
one.** Twenty-four entries across `cpu`, `io`, `mem`, `net`, `gpu`, `proc` and
`power` declare both `snapshot` and `poll`, and every one of them is a session
away from the same silence. What *is* local to this family is the second half:
`power.temp` is the only collector in `src/collect/` whose `tick()` returns
`Some(ZERO)`, which is what collapses the window's interval and manufactures
the fictitious late ticks. Elsewhere the snapshot would simply be served as a
poll, quietly.

`examples/power-every-axis.yml` therefore leaves the snapshot out and says so;
the sweep exercises it from the command line, where it is correct.

### 4. A session is driven at its first request's pace, whatever the others ask for

> **Fixed** (stream A). Each request keeps its own cadence, in either order, and
> the related behaviour this entry is careful to separate is untouched: a
> collector that declares a tick still keeps it, because a ring drained less
> often than it fills loses events. Asserted by
> `two_requests_at_two_rates_are_each_served_at_their_own`. The sessions this
> family ships were written so the defect could not bite them, and they are left
> as they are.

When no collector declares a pace of its own, the runner falls back to
`resolved[0].request.mode` — the **first** request's interval — and applies it to
every request in the session. Every `power` collector takes the default
`tick() == None` (`power.temp` returns a value only in snapshot mode), so this
family meets it directly. Both directions, on the same two requests:

```console
$ cat slow-first.yml                          $ cat fast-first.yml
requests:                                     requests:
  - {label: slow, source: power.temp,           - {label: fast, source: power.freq,
     mode: poll@500ms, ...}                        mode: poll@100ms, ...}
  - {label: fast, source: power.freq,           - {label: slow, source: power.temp,
     mode: poll@100ms, ...}                        mode: poll@500ms, ...}

$ omn -f slow-first.yml                       $ omn -f fast-first.yml
  slow   3 events over 1.01s                    fast   168 events over 606.12ms
  fast   3 events per cpu over 1.01s            slow     7 events over 606.12ms
```

`poll@100ms` served at 500 ms on the left; `poll@500ms` served at 100 ms on the
right. The harmful direction is the left one — a request read five times less
often than it asked, over a window that also overran its own duration by 70 %,
because the deadline is only checked once a full slow tick has elapsed. The
resolved record written beside the results still says `poll@100ms`, so nothing
in the output says the spacing is not the one that was requested.

The related behaviour is **not** a defect and should not be confused with it:
when a collector *does* declare a tick, the fastest collector sets the pace for
the whole run, which `drive()` documents and intends. Adding a counted
`cpu.cycles` request to `examples/power-core-under-load.yml` is what that looks
like — the polled requests say `poll@100ms` and are read about every ten
milliseconds. Each event still carries the `interval_ns` it covers, so nothing
is wrong; it is simply not the tick that was asked for, and the session's header
says so.

**Also for the coordinator rather than for this family.** Nothing about it is
about `power`: any session that mixes polled intervals across sources whose
collectors declare no pace of their own gets one of those intervals and is not
told which. The sysfs pollers are the exposed ones, so `io`, `mem` and `proc`
have the same exposure. Of the coordinator's own sessions,
`examples/saturation.toml` is safe by accident — every one of its five requests
asks for `poll@1s` — and `examples/web-latency.yml` is safe because its
collectors all declare a tick. Neither is safe by design, and a request added
to either would decide it.

Every session this family ships is written so that the defect cannot bite it:
`power-energy.yml` leads with its polled request rather than its counted one and
explains why, and `power-every-axis.yml` gives every request the same interval.
Both say so in their headers, because a reader who copies a session and adds a
request to it needs to know.

### 5. Not this family's, but this wave will meet it: a unit test that fails under load

`collect::tests::what_a_run_reports_spending_is_the_cpu_it_actually_burned`
forks a child, spins a collector for 200 ms and refuses to conclude anything
unless at least 150 ms of that was actually burned. On a machine with cores to
spare it is right to do that. On a busy one the child is descheduled, burns
less than its floor, and the test fails with "the collector was asked to spin
for 200 ms and did not, so this measured nothing".

It is in `src/`, untouched by any scenario stream, and it fails from
`cargo test --lib` alone -- a binary built from `src/` and nothing else:

```console
$ for i in $(seq 6); do cargo test --lib | grep "test result"; done
test result: ok. 660 passed; 0 failed
test result: ok. 660 passed; 0 failed
test result: FAILED. 659 passed; 1 failed
test result: ok. 660 passed; 0 failed
test result: ok. 660 passed; 0 failed
test result: ok. 660 passed; 0 failed
```

It is recorded here because this wave makes it worse rather than because this
wave caused it. The load average on the machine those six runs were taken on
was twenty, from eight scenario streams running their suites at once, and the
scenarios that carry this family's ground truth put every core under load on
purpose. Two ways out, and they are the coordinator's call: make the floor a
fraction of the cpu time the child actually got rather than of the window, or
have the child pin itself and assert against its own `getrusage`. Either keeps
what the test is for -- that a budget cannot be built on a collector that
under-reports its own cost -- without asking the machine for a core it may not
have.

---

## Running them

```console
make                                    # the capabilities do not survive a relink
cargo test --test scenario_power
omn -f examples/power-every-axis.yml
```

Fourteen scenarios, **7.6 s** in parallel and 17 s in sequence. Five of them
are over a second and here is each one's reason; the other nine are under, and
six of those are under a tenth.

| Scenario | Wall | Why it cannot be shorter |
|---|---|---|
| `every_axis_this_family_declares_is_served_or_refused_by_name` | 5.0 s | thirty-six subprocess runs, and two of them are counted energy windows that cost a second each whatever `--duration` says — which is defect 1 showing up as a bill |
| `a_machine_with_every_core_busy_costs_more_than_an_idle_one` | 2.6 s | two counted seconds, for the same reason |
| `every_session_this_family_ships_validates_and_then_runs_or_refuses_by_name` | 2.2 s | seven sessions, spawned at once rather than in turn — nine seconds in sequence |
| `the_frequency_of_a_busy_core_matches_the_cycles_that_core_issued` | 2.1 s | 700 ms for the load to settle, then a 1 s window; a shorter window has too few `cpuinfo_avg_freq` samples to mean anything |
| `a_tick_shorter_than_a_round_of_reads_is_counted_and_surfaced` | 1.7 s | three 500 ms windows: the hurried session, and the comfortable one it is contrasted with |
| `a_core_held_busy_never_enters_an_idle_state` | 1.1 s | 400 ms for the load to settle, then four 150 ms ticks |

Four of them hold a process-wide lock because they need the machine to
themselves — two put every core under load on purpose, and run at the same time
they measure each other: a core "pinned" to one spin loop is shared with
twenty-four more, and an idle baseline is taken while the machine is not idle.
The lock is the scenario's, not the tool's. What it protects is the property
the workload chose, which is the whole of the measurement.

One scenario skips on an ordinary machine, and names what it wants: the RAPL
backend comparison needs root, because `/sys/class/powercap/intel-rapl:0/
energy_uj` is root-only. Nothing else skips here. On a machine without
`taskset`, without cpufreq or cpuidle, or without a RAPL domain, the scenarios
that need them skip naming exactly that, and their sessions stay in `examples/`
for a machine that has them.
