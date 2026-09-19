# Open findings

Cross-cutting findings from the family threads, kept here because the notes they
came from were untracked and are gone. Each one is reproducible, names who it
belongs to, and is not a task list: several need a decision before they need
code.

---

## 1. The catalog declares per entry what actually varies per backend

Three threads hit this from three directions in one session, which is usually
the sign of a real modelling gap rather than three inconveniences.

**Targets.** `catalog::Backend` carries `id`, `requires`, `cost`, `degraded`,
`equivalent` and `join` — and nothing about which targets it can serve. The
resolver only knows the entry-level `targets` map. So `io.bytes --system`, whose
chain is `bpf:tp` → `fs:cgroup` → `fs:proc`, resolves onto `fs:cgroup`, a
backend whose whole purpose is per-cgroup attribution. Reordering the chain
would fix that request and break the opposite one: `--cgroup` would land on
`/proc/diskstats` and be answered with the whole machine's I/O, which is the
worst bug this tool can have.

**Filter fields.** Of the 42 sources with a collector, 26 declare a
`filter_fields` entry that no collector in that family emits. Since the
userspace filter now actually runs, a predicate over such a field is false
everywhere and empties the series. The run says so — "no event carried `cpu`,
so that part of the predicate was never true" — but the catalog is promising
something the build does not deliver.

| Family | Sources | Field(s) never emitted |
|---|---|---|
| `cpu` | cycles, instructions, ipc, branch_misses, cache_misses, context_switches, migrations | `comm` |
| `mem` | rss, pss, uss, faults, heap.alloc | `comm` |
| `io` | bytes, iops, merges, inflight | `dev`, `rwbs`, `comm` |
| `net` | bytes | `comm` |
| `sched` | wakeup, switches | `pid` |
| `power` | freq, cstate, throttle | `cpu` |
| `proc` | fd, threads | `comm` (fixed: both now emit it) |
| `runtime` | jit | `comm` |

Some of these are per-backend too: `comm` on `net.bytes` is real for the eBPF
backend and impossible for `/proc/net/dev`, which has no idea which process
moved what.

**What to do.** Either give `Backend` its own `targets` and `filter_fields`,
falling back to the entry's when absent, or accept that an entry describes the
union and make the resolver and the validator say which backend drops what.
The first is more honest and more work. Until then, per source, the choice is to
emit the attribute or take the field out of the catalog — and guessing which the
author meant is not something to do for them.

`src/collect/gpu/device.rs` has the test shape worth copying:
`every_filter_field_the_catalog_offers_is_on_the_events` reads the catalog
entry, samples once, and asserts each declared field is on the event. Twelve
lines, and it catches every row of that table.

---

## 2. There is no key for a device or a cpu

`KeyValue` gained `Iface` for `net`. Nothing exists for a block device or a cpu,
and `emit::grouped` keys a series on the event's first key.

So the four `io` diskstats sources emit one event per tick holding the sum over
every leaf device, with a `devices` attribute naming what was summed. The sum is
correct — leaf devices do not overlap — but a machine with an NVMe and a
spinning disk reports one number for both, and the `topk` over devices that
`io.bytes` and `io.iops` declare cannot be expressed at all. `power` has the
same gap per cpu: `--cpu` narrows the reading, but a per-cpu series is
impossible, which is why frequency reports a mean with `min_hz`/`max_hz` beside
it. `cpu.util` refuses `--cpu` for the same reason rather than answering with
the machine's average.

Wanted, in the shape `Iface` already has:

```rust
KeyValue::Dev { major: u32, minor: u32 }   // dev:259+0
KeyValue::Cpu { cpu: u32 }                 // cpu:7
```

The `dev` filter field four `io` entries already declare would then have
something to match against.

---

## 3. A skipping test is not a passing test

This cost three threads time in one session, in three different shapes, and it
is the most transferable lesson of the lot.

- A `cpu` test asked for `--emit raw` on a source that did not declare `raw`,
  and skipped whenever the command failed on the assumption that a failure means
  a machine that cannot count. The request had been **refused** since the test
  was written: it passed, in green, for hours, checking nothing.
- Two `power` tests skipped any source `omn list` did not call "usable", and
  `power.temp` reports as *assumed rather than checked*. They passed without
  running.
- An `io` ground-truth test needed a real block device and used `stat` to find
  one; btrfs gives its subvolumes an anonymous `st_dev` that appears nowhere
  under `/sys/dev/block`, so it silently skipped on the only machine that could
  run it.

A machine that cannot serve a request and a request that cannot mean anything
are different answers, and only the first is a reason to stop quietly.
`tests/family_cpu.rs` has the discriminator:

```rust
fn machine_cannot_serve(message: &str) -> bool {
    message.contains("no viable backend")
        || message.contains("cannot start collecting")
        || message.contains("perf_event_paranoid")
}
```

Anything that can skip should be able to say, in its message, what it would have
checked.

---

## 4. Losses that are not counted as losses

**A throttled perf event.** `PERF_RECORD_THROTTLE` is decoded by `SampledEvent`
and ignored by `src/collect/mem/page_faults.rs`, so a throttled run looks
complete. `sched.switches` counts it into `Losses::dropped` with a reason naming
`kernel.perf_event_max_sample_rate`. The shared layer should expose
`SampledEvent::throttled()`, and `Losses` should carry a `throttled` count of
its own: the number of events lost inside a throttled period is not known, and
folding it into `dropped` overstates precision.

*Half done.* `Losses::throttled` and `Losses::throttle(reason)` exist, kept
apart from `dropped`, and a non-zero count makes a run not clean. Nothing
populates them yet: the two producers and `SampledEvent::throttled()` are in
files stream F does not own, and are asked for under "Needs from another
stream" below.

**The closing read of a polled counter.** `net/dev.rs::stop` takes a final
reading, on the grounds that without one the last interval of every run is
dropped. `io` and `power` deliberately do not, because `run_all` samples every
collector and *then* decides whether to stop, so a closing read covers only the
few microseconds since the last tick — in `io` it produced a second point at the
same offset with a value of zero, dragging every summary after it. Both cannot
be right. Either the runner stops before its last sample rather than after it,
or no counter collector reads at the edge. Today a reader comparing two families
gets a different convention in each.

*Decided, and half applied.* The runner already closes the window: it reads
every collector and only then asks whether the run is over, so the last round
of reads lands at the end. `io` and `power` are right, `net/dev.rs` is not, and
`DECISIONS.md` now says so under "the edges of a run". `Collector::stop` states
the contract and `the_runner_closes_the_window` measures the leftover the
convention rests on — 48 µs of a 100 ms tick. The one-line change to
`net/dev.rs` is asked for below.

---

## 5. Smaller, each with its reason

- **An exit code for "valid, but this mechanism is not built".** A mode a
  collector cannot serve returns `CollectError::Start` and exits 1, like a real
  failure. `NoCollector` exits 3 and this belongs in the same class. (Partly
  done: `UnsupportedTarget` joined `NoCollector` at exit 3, and is now asked
  before the "pass --duration" refusal, so `mem.heap.alloc --pid 1` says what
  is actually wrong. A *mode* a collector cannot serve still arrives as a bare
  string and cannot be told apart from a mode that can never work; the
  signature that would fix it is under "Needs from another stream".)
- **`count` → `raw` in the mode × emit matrix.** Added by the `cpu` thread so a
  counted source's own attributes — the terms of `cpu.ipc`, the `enabled_ns` and
  `running_ns` every counter reports — can reach a user. A counted window *is*
  one event, so `raw` is the emit that shows it. It is a model decision
  documented only in `docs/families/cpu.md`: promote it here or revert it.
- **A rate over the counter's own window.** `emit::rate` divides by wall clock;
  a counted source knows exactly how long it was enabled and puts it on the
  event as `enabled_ns`.
- **`Unit::Watts` does not exist**, which is part of why `gpu.power` reports the
  energy counter rather than an instantaneous draw.
- **The GPU is not in the environment capture**, and two runs are comparable
  only if they were taken on the same device under the same driver. Not free:
  `nvmlInit` costs 3.3–4.9 ms and `Capabilities::detect` runs on every command,
  including those that measure nothing. (Done, and not through NVML:
  `Environment::gpus` reads `/sys/class/drm/cardN` for the PCI ids, the bound
  driver and `/sys/module/<driver>/version`. The fastest of twenty walks is
  47 µs here, so the capture is unconditional and the request never had
  to be consulted. What it costs is the marketing name — `0x10de:0x1e07`, not
  "RTX 2060" — which needs a PCI id database this tool does not carry and which
  no comparison needs. The userspace half, NVML and CUDA versions, is now named
  in `not_captured`.)
- **`hwmon` is answerable and still unverified.** `Capabilities::probe` ends
  with a family extension point; `power` can answer it by listing
  `/sys/class/hwmon` for a chip with a `tempN_input`. (`pmu:power/energy-pkg/`
  was the other half of this and is done: the encoding is read from sysfs and
  the domain is opened, so `power.energy` collects through perf without root.)

---

## 6. Measurements not worth deriving again

From the threads that made them, on the development machine.

- **Memory the allocator already holds is already resident and faults nothing.**
  Three identical `Vec` allocations gave 64, 64, then **0** faults. Any test
  whose premise is "touching memory faults" must `mmap(MAP_ANONYMOUS)` its
  pages. This, not huge pages and not loss, is what made the fault sampler look
  flaky.
- **A fault is not a page.** With multi-size THP one fault installs a folio, so
  the count is a fraction of the pages touched: 263 faults for 1 MiB, 528 for
  32 MiB, from 3.9 KiB per fault up to 62 KiB.
- **A waker can be the task that was merely interrupted.** A process doing
  nothing but `sleep 0.3` appeared to wake `rcu_preempt`, a kworker and a
  network daemon: softirqs run in the interrupted task's context, and a waker of
  pid 0 is the idle task. Any `WAKEUP` join is only as good as the waker.
- **Per-process VRAM from NVML is exact** — 256 MiB allocated through the CUDA
  driver API came back as 268435456 bytes — but an empty CUDA context already
  holds 163.6 MB. VRAM held is an arena, not usage.
- **Integrating NVML's sampled power does not reproduce the device's energy
  counter**, and polling faster makes it worse: ratio 0.90–0.97 at 1 Hz,
  0.64–0.75 at 20 Hz. The driver returns an already-filtered value.
- **RAPL reads fine through perf as an ordinary user here** where sysfs will not
  show it without root: type 14, config `0x02`, `pid = -1`, cpu 0, two reads a
  second apart gave 145.5 W of package power.
- **A preload shim costs 132 ns per call against 740 ns for an eBPF uprobe** on
  the same event, and the uprobe needs privileges CI runners do not grant.
- **`power.cstate` can report an idle share above 1.0 for one tick.** The kernel
  credits residency when a cpu *leaves* an idle state, so a tick in which a long
  sleep ended is paid time belonging to earlier ticks. Over two seconds it reads
  0.968 against `/proc/stat`'s independent 0.974. Reported as it is, because
  clamping it would hide the effect and break the sum.

---

## Needs from another stream

Written by stream F, which owns `src/collect/mod.rs`, `src/main.rs`,
`src/capabilities.rs` and `src/cli.rs`. Each of these is the other half of
something landed there; the half that is missing is in a file another stream
owns, and the shape wanted is exact so that landing it is a small edit rather
than a design.

### To whoever owns `src/perf.rs` (stream B holds its callchain)

```rust
impl SampledEvent {
    /// Throttled periods seen since the event was opened, accumulated across
    /// drains the way `lost()` is.
    pub fn throttled(&self) -> u64;
}
```

`Record::Throttle { throttled: true }` already reaches `drain`'s handler, and
the two collectors that see it do different things with it. An accumulator
beside `lost` and `undecoded` means a sampling collector reports throttling by
default rather than by remembering to, which is how `mem/page_faults.rs` came
to ignore it. `PERF_RECORD_UNTHROTTLE` closes a period and must not be counted
as a second one.

### To `src/collect/mem/page_faults.rs` and `src/collect/sched/switches.rs`

`Losses::throttle(reason)` now exists and increments `Losses::throttled`, a
count kept apart from `dropped` (see `DECISIONS.md`, "the edges of a run").

- `page_faults.rs:109` has `Record::Throttle { .. } => {}`. A throttled run of
  `mem.faults` reads as complete today. It should call
  `self.losses.throttle(...)` naming `kernel.perf_event_max_sample_rate`.
- `switches.rs:103` adds the throttle count to `losses.dropped`. That says "N
  events were lost", which is a number nobody has; it should move to
  `losses.throttle(...)` and leave `dropped` for what the ring actually
  reported.

### To stream A, for `src/encode.rs`

The text `LOSS:` line at `encode.rs:258` prints `dropped`, `failed_reads` and
`late_ticks`. It omits `undecoded`, and now also `throttled`. Both reach the
reader only through the `reasons` list underneath, which is one indirection
away from the number:

```rust
"  LOSS: {} dropped, {} failed reads, {} late ticks, {} undecoded, \
 {} throttled periods"
```

A throttled or undecoded run is already not `is_clean()`, so the line does
print; it just prints five zeroes and a reason. "Losses are counted and
surfaced to the user, always" is the invariant this is one word short of.

### To `src/collect/net/dev.rs`

```rust
fn stop(&mut self) -> Vec<Event> {
    Vec::new()
}
```

`net/dev.rs:205` takes a closing read on the grounds that the last interval of
every run would otherwise be dropped. It is not: the runner reads every
collector and *then* decides to stop, so the last round of reads already lands
at the end of the window. Measured, on this machine:

```console
$ omn net.bytes --iface lo -d 350ms -m poll@100ms -e timeline --format json
offsets: 0, 100058536, 200122645, 300169542, 300217653
```

The fifth point is 48 µs after the fourth, with a delta of zero. It is in every
`net.bytes`, `net.packets` and `net.iface` series and it drags every mean
computed over one. `io/diskstats.rs:370` and `power/energy.rs:305` document the
opposite convention and are the ones that are right. The unit test
`the_last_interval_of_a_run_is_measured_rather_than_dropped` in that file calls
`sample` twice by hand and so passes either way; what it is checking is that
`sample` differences correctly, which it should keep doing under its own name.

### To stream D, for `src/catalog.rs`

```rust
pub enum Unit {
    // ...
    Watts,
}
```

`gpu.power` reports the device's energy counter because there is no unit for an
instantaneous draw, and `power.energy` has the same shape. Energy is the right
thing to *collect* -- integrating a sampled draw overstates it by a third at
20 Hz, which `DECISIONS.md` settles -- but a reader asking "how much power is
this drawing" wants watts, and `rate` over joules is exactly that. The unit is
the missing half.

### To whoever owns `src/render.rs`

`push_environment` at `render.rs:156` lists the environment field by field, so
the graphics devices now captured in `Environment::gpus` reach `resolved.json`
and not the text dump. One line beside the `clocksource`/`governor`/`thp`/`smt`
row:

```
gpu       card1 0x10de:0x1e07 nvidia 615.71.09
```

Two runs are comparable only if they were taken on the same device under the
same driver, and the text dump is where a reader checks that before trusting a
comparison.

### To whoever owns `CATALOG.md`

Line 418 of the global options table still reads:

```
--budget 2%                 overhead budget; the resolver degrades and says so
```

The resolver does not. `man/omn.1` says "Parsed and recorded; not yet
enforced", and the CLI help and the run-time note now say the same; this is
the last place that promises the behaviour. One line.

### To whoever changes `Prepared`'s factory signature

`Factory`'s `build` and `Prepared::attach` both return `Result<_, String>`, so
every refusal from a collector arrives as `CollectError::Start` and exits 1.
Two unlike things are in there:

- `cpu/counters.rs:154` -- "`sample` mode needs the sampling path, which lands
  with stacks". That is *not built yet*, and belongs at exit 3 with
  `NoCollector`.
- `power/energy.rs:45` -- "energy is a counter that has to be differenced over
  a window: `sample` has none". That can never work, and belongs at exit 2.

Telling them apart needs the collector to say which it means. A richer error
type with `impl From<String>` would keep every existing `?` compiling, so the
change is one enum plus the handful of refusals that want the other variant.
`main.rs::not_built_yet` is where the classification is read.
