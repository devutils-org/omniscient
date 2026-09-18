# Family `power`

Five sources that matter out of proportion to their size, because they are the
confounder for every other family. A processor that throttled, or that ran at a
lower frequency under one instruction mix than under another, moves a cpu, a
memory and a gpu measurement at once, for a reason that appears in none of them.
The roadmap's intent is that these are collected at all times, whether or not
anyone asked, and that a throttled run is marked unusable rather than averaged
in.

A run that throttled is not a slower run. It is a different run.

## What is collected

Everything here reads sysfs. The `pmu` backends -- RAPL and the aperf/mperf pair
through `perf_event_open` -- are the better instruments and wait for counting
mode in the shared perf layer; the last section says what they need and what
they are worth.

| Source | Backend | What an event carries |
|---|---|---|
| `power.energy` | `fs:sys` (powercap) | joules per top-level RAPL domain over the tick, and the watts that implies |
| `power.freq` | `fs:sys` (cpufreq) | the frequency of the watched cpus, and whether it is the effective or the requested one |
| `power.cstate` | `fs:sys` (cpuidle) | idle residency per state over the tick, per cpu and in total |
| `power.temp` | `fs:sys` (hwmon) | the hottest processor sensor, with every sensor beside it |
| `power.throttle` | `fs:sys` (`thermal_throttle`) | throttling events during the tick, or a refusal where they cannot be seen |

## What the implementation had to decide

**There is no such thing as "the" temperature.** This machine offers a processor
at 56 °C, an NVMe controller at 44 °C and a wifi chip at 52 °C, and their mean is
a number about nothing. The value of an event is the hottest *processor* sensor,
because that is what throttling follows; `sensor` names which one it was and
`basis` says how it was chosen. Every sensor is carried as its own attribute, so
nothing is hidden by the choice. On a machine with no processor sensor the value
falls back to the hottest sensor of any kind and `basis` says so, rather than the
number quietly changing meaning between two machines.

Which chip is a processor comes from the driver name -- `k10temp`, `coretemp`,
`zenpower`, `cpu_thermal` -- and not from the label, because the labels are
vendor-specific (`Tctl` and `Tdie` against `Package id 0` and `Core N`) while the
driver is what says whose silicon it is.

**"The frequency" is two different numbers and sysfs will hand over either.**
The requested frequency is what the governor asked for; the effective frequency
is what the core ran at, and they part company exactly when it matters -- under a
power or thermal limit, or under an instruction mix that carries a frequency
licence. So the file is chosen once, at the start, in this order:
`cpuinfo_avg_freq` (an average over the interval, computed by the processor's own
performance counters), then `cpuinfo_cur_freq` (read back from the hardware),
then `scaling_cur_freq` (what the kernel believes). Every event carries the
`driver`, the `basis` naming the file, and `measures`, which is `effective` or
`requested`. On this machine, with `amd_pstate`, `cpuinfo_avg_freq` exists and
the number is effective -- which is why the catalog's old blanket note, "requested
frequency, not the effective one", was replaced: it was false here, and a
degradation that is false is worse than none.

A system-wide reading is the mean across the online cpus, with `min_hz`, `max_hz`
and every cpu's own value beside it. One boosted core among twenty-three idle
ones is a mean that describes no core, and `--cpu` is how to ask about a core.

**Idle residency is credited late, and the arithmetic looks wrong before it
looks right.** The kernel adds the time to a state's counter when the cpu
*leaves* it, so a tick in which a long sleep ended is paid residency that belongs
to earlier ticks. Measured here: one 300 ms tick reported 1.06 of itself as idle.
Rather than clamp it -- which would hide the effect and break the sum -- the
number is reported as it is, the caveat says why, and the ground-truth test uses
a window long enough for the lag to wash out.

Residency is also cpu-time, not wall time. Reported as a total, a 300 ms tick on
twenty-four cpus reads `5.99s`, which looks like a mistake. The value is
therefore the residency of the *average* watched cpu, which can never exceed the
tick, with `idle_ns_total` and `idle_ratio` beside it. With `--cpu 3` the two are
the same number.

**RAPL nests, wraps, and is root-only.** `intel-rapl:0:0` is the cores inside
package `intel-rapl:0`; adding both counts the cores twice, so only top-level
domains are summed and the event names them -- the same shape as partitions and
disks in the `io` family. The counter wraps at `max_energy_range_uj`, read from
the domain rather than assumed, which is 65 kJ here: about eleven minutes at a
hundred watts. A decrease is read as one wrap and the event says `wrapped`; a
tick long enough to wrap twice cannot be recovered, and the loss reason says so
instead of reporting the remainder.

**A machine that cannot see throttling must not report none.** `thermal_throttle`
is an x86 thermal-interrupt interface. It is absent on AMD parts, on ARM and in
most virtual machines, and this is an AMD machine. A series of zeroes there reads
exactly like a run that did not throttle, which would defeat the one source whose
job is to mark a run unusable. So the collector refuses to start, by name, and
the resolver refuses earlier still -- the catalog requires the file, and `omn
list power` shows the source as out of reach here.

## What was measured

```console
$ omn power.temp --system -m poll@300ms -e timeline -d 1s
  timeline
    system
      +0ns        59 celsius
      +305ms      56 celsius
      ...
  unverified requirement: hwmon

$ omn power.cstate --system -m poll@300ms -e timeline,raw -d 900ms
  raw
    ... idle_ns=262352041 idle_ns_total=6296449000 idle_ratio=0.8738 entries=2008
        interval_ns=300230668 cpus=24 state.c1_ns=25620000 state.c2_ns=6270516000
        state.poll_ns=313000

$ omn power.throttle --system -m poll@300ms -e timeline -d 1s
error: `power.throttle` has no viable backend on this machine:
  bpf:tp: tracepoint:thermal:thermal_temperature: missing: tracefs is mounted but not
    readable by uid 1000 ...
  fs:sys: /sys/devices/system/cpu/cpu0/thermal_throttle: missing: does not exist
```

The idle line is the interesting one: 6.30 cpu-seconds of idle in a 300 ms tick
across 24 cpus, 87 % of the machine, almost all of it in C2 -- and the *next*
tick reported 106 %, which is the crediting lag rather than an error.

## Ground truth

Each source is held against something it did not compute:

- **`power.cstate` against `/proc/stat`.** Idle and iowait jiffies are a second,
  independent accounting of the same thing, sharing no code with cpuidle. Over
  two seconds, cpuidle said 0.968 of the machine was idle and `/proc/stat` said
  0.974. The test compares them over 1.2 s, with a tolerance that covers the
  ten-millisecond jiffy per cpu, and holds on a saturated machine too, where both
  are near zero and a ratio would divide by nothing.
- **`power.temp` against the thermal zone interface.** Where a zone's `type`
  names a chip hwmon also exposes, the two paths must agree within a few degrees:
  they are the same silicon. On this machine the wifi chip appears in both.
- **`power.freq` against the kernel's own declaration.** Every cpu's reading has
  to lie inside `cpuinfo_min_freq`..`cpuinfo_max_freq`, which the driver
  publishes. A kilohertz left unconverted fails this by three orders of
  magnitude.
- **`power.energy` and `power.throttle` against a synthetic `/sys`.** Neither can
  be read on this machine, so the arithmetic that matters -- a wrapped counter, a
  nested domain, a throttle delta -- is exercised against a tree the test writes,
  with the values this machine's hardware publishes. The live tests check the
  refusals instead: that an unreadable counter asks for privileges, and that the
  collector and the filesystem agree about whether throttling can be seen here.

What is still missing is an *injected* confounder: pinning the frequency, forcing
a package power limit and checking that the collectors see it. That needs root
and a machine that can be perturbed, so it belongs with the phase 8 harness.

## Needs from the shared layer

### 1. A probe calls an unreadable file readable

`capabilities.rs` decides what a path is with `std::fs::metadata`:

```rust
fn access(path: &Path) -> Access {
    match std::fs::metadata(path) {
        Ok(_) => Access::Readable,
        ...
```

`stat` succeeds on a file the caller cannot open: it needs search permission on
the directories, not read permission on the file. So a root-only counter under a
world-readable directory is probed as **available**, and the resolver promises a
measurement the collector then cannot take:

```console
$ omn list power.energy
    2. fs:sys                 cost free
       needs    /sys/class/powercap/intel-rapl:0/energy_uj available
$ omn power.energy --system -m poll -e timeline -d 1s
error: cannot start collecting `power.energy`:
  /sys/class/powercap/intel-rapl:0/energy_uj is readable only by root on this kernel
```

Confirmed directly: `stat` succeeds, `access(R_OK)` is false, `open` returns
`EACCES`. This is the "restricted is never absent" invariant being defeated by
one call -- and it is not specific to `power`: every catalog requirement that
names a file this user cannot read is affected, which on a typical machine means
most of the MSR, tracing and energy paths.

Wanted: `access` should ask whether the file can be *read*, with
`faccessat(AT_FDCWD, path, R_OK, AT_EACCESS)` or by attempting to open it, and
keep `Restricted` for exactly that case. The distinction already exists in the
type; only the test behind it is wrong.

### 2. Counting mode, and an arbitrary PMU, in the perf layer

`src/perf.rs` opens one software event on one task and samples it. The two best
instruments in this family need neither of those things: they need a counter,
opened on a PMU named in sysfs, read system-wide.

It is worth being concrete about what is lost. The energy counter this family
cannot read through sysfs without root **is readable through perf as an ordinary
user on this machine**, where `perf_event_paranoid` is -1. Opening
`/sys/bus/event_source/devices/power` type 14, config `0x02`, on cpu 0 with
pid -1, and reading it twice a second apart, returns 145.5 W of package power.
The sysfs path for the same number is mode 0400.

The same mechanism gives the effective frequency without any privilege at all:
the `msr` PMU exposes `aperf`, `mperf` and `tsc` as counters, and their ratio is
what `cpuinfo_avg_freq` computes internally -- available on every driver, not
only on `amd_pstate`.

Wanted, in the shared perf layer:

```rust
/// A counter, not a sampler: opened, read at each tick, never mapping a ring.
pub struct CountedEvent { /* ... */ }

impl CountedEvent {
    /// `pmu` is a directory under /sys/bus/event_source/devices, whose `type`
    /// file gives the attr type and whose `events/<name>` file gives the config.
    pub fn open_pmu(pmu: &str, event: &str, scope: Scope) -> io::Result<Self>;
    pub fn read(&self) -> io::Result<u64>;
    /// The `.scale` and `.unit` files beside the event, which say what the
    /// counter's units are -- RAPL counts in 2^-32 joules, and a collector that
    /// assumed microjoules would be wrong by ten orders of magnitude.
    pub fn scale(&self) -> Option<(f64, String)>;
}
```

With it, `power.energy` gains its first-choice backend on every machine where
perf is permitted, and `power.freq` gains an effective-frequency backend that
does not depend on the driver.

### 3. Probe classes for `hwmon` and for a named PMU event

`omn power.temp` runs and reports `unverified requirement: hwmon`, because no
probe covers that class. It is one directory listing. The same is true of
`pmu:power/energy-pkg/`, which the probe answers with "needs an event encoding
this tool does not build yet" although the encoding is sitting in
`/sys/bus/event_source/devices/power/events/energy-pkg` -- along with its scale
and its unit.

Wanted in `capabilities.rs`: `hwmon` is available when `/sys/class/hwmon` holds
at least one chip with a `tempN_input`; `pmu:<pmu>/<event>/` is available when
`/sys/bus/event_source/devices/<pmu>/events/<event>` exists, and missing with the
name of what was looked for when it does not. Both are cheap, and both replace an
"unverified" that currently travels all the way to the output.

### 4. The guardrail is a session-level mechanism, not a collector

The brief asks for more than five collectors: "the mechanism by which a throttled
run is marked unusable". That cannot live in this family, because it has to act
on the *other* families' results. The pieces this family can offer are in place
-- throttling refuses rather than reporting zero, frequency says whether it is
effective, temperature names its sensor -- but the hook is missing.

Wanted, at session level: `power.throttle` and `power.freq` collected alongside
whatever was asked for, without being requested; and a field on the results that
says the run was disturbed, with what was seen -- so that a comparison between
two runs can refuse to be made. Its shape matters for phase 8, where the harness
invalidates runs automatically, and for `--budget`, which will need somewhere to
record a degradation that was not the user's choice.

### 5. A frequency reads better in gigahertz

`quantity` special-cases bytes, nanoseconds and ratios; hertz comes out as
`3962428538 hz`. The `net` thread has already had one unit-rendering fix land
("render a column of durations in one unit"), so this is the same small piece of
work: a scaled rendering for `Unit::Hz`, and the same for `Unit::Joules`, whose
values here span a tick's millijoules to a run's kilojoules.
