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

**The closing read of a polled counter.** `net/dev.rs::stop` takes a final
reading, on the grounds that without one the last interval of every run is
dropped. `io` and `power` deliberately do not, because `run_all` samples every
collector and *then* decides whether to stop, so a closing read covers only the
few microseconds since the last tick — in `io` it produced a second point at the
same offset with a value of zero, dragging every summary after it. Both cannot
be right. Either the runner stops before its last sample rather than after it,
or no counter collector reads at the edge. Today a reader comparing two families
gets a different convention in each.

---

## 5. Smaller, each with its reason

- **An exit code for "valid, but this mechanism is not built".** A mode a
  collector cannot serve returns `CollectError::Start` and exits 1, like a real
  failure. `NoCollector` exits 3 and this belongs in the same class.
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
  including those that measure nothing.
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

## 7. What the aggregation layer found once `hist`, `topk` and `stack` existed

From the stream that wrote them. The emits are in, the `folded` and
`speedscope` encoders are in, and three of the things they touch belong to
somebody else.

**A series has a name now, and two families cannot produce the ones they
declare.** `emit::SeriesKey` is what `topk` ranks and what `timeline`,
`summary`, `rate` and `hist` produce one of each per: every event sharing the
first key its collector put on it, or `system` when it has none. That makes
finding 2 concrete rather than theoretical. `io.bytes` and `io.iops` declare
`topk` and the `fs:proc` backend emits one event per tick holding the sum over
every device, so `topk` over them answers with one series called `system` —
true, and not the question. `power` is the same per cpu. Both are answered by
`KeyValue::Dev` and `KeyValue::Cpu`; nothing in the emit layer needs to change
when they arrive, because a series is keyed on whatever the collector put
first.

**A log2 histogram is the layout, and that was a choice about the kernel.**
Bucket 0 holds the values below one and bucket *k* holds `[2^(k-1), 2^k)` —
what `bpf_log2l` plus an array map computes, one index and one atomic increment
per event, no record. A prettier layout would have to be abandoned when
`--budget` moves the aggregation in-kernel (section 5, and `STATE.md`'s item on
the budget), and two runs of the same source across that change would not be
comparable. The validator already warns that `trace` with `hist` aggregates in
userspace without a budget; that warning is now about something real.

**`request::HistScale` cannot be serialized, so the emit layer carries its own
copy.** `Emit` and `HistScale` are `Copy, PartialEq, Eq` and nothing more, and
a result file has to say how to read its buckets, so `emit::BucketLayout`
exists with the same two variants and a string form. If the catalog-model
stream gives `HistScale` `Serialize`/`Deserialize`, `BucketLayout` collapses
into a re-export. It is duplication, not a disagreement: the conversion is a
`From` impl and the two cannot drift without the compiler saying so.

**`hist:linear:0` parses and should not.** `Emit::from_str` refuses a negative
width and a missing one — "a linear histogram needs a bucket width, as in
`hist:linear:1ms`" — and accepts zero, because the width goes through
`i64 -> u64` and zero converts. Every value then divides into one bucket of
zero width. The emit layer refuses it with a reason rather than rendering it,
but it refuses *after* the run, which throws a real measurement away for a typo
that was visible before anything was attached. It belongs with the other two
width errors in `src/request.rs`, which is the catalog-model stream's file.

**`sched.switches`'s ground-truth unit test fails when the suite is loaded.**
`collect::sched::switches::tests::the_switch_count_agrees_with_the_kernels_own`
counted 21 switches against the kernel's own 56, with nothing reported as lost,
while the rest of the library suite ran beside it; run alone it passes every
time. The test asserts `measured * 10 >= kernel * 8` on a machine where the
reference counter is moving for reasons the test did not cause, which is the
sampler being right and the reference being wrong rather than the other way
round. It belongs to whoever owns `sched`; it is not the aggregation layer, and
it is a test that is green on an idle machine and red on a busy one, which is
the worst kind to leave.

---

## Needs from another stream

### The stack seam, for stream B

Agreed shape, and the half of it that is written: aggregation, the side table,
the `folded` encoder and the `speedscope` sampled profile all exist and are
tested. What they read is two attributes on an ordinary `Event` — no new field
on `Event`, and nothing added to `Collection`, so this needs no change in any
file stream B does not own.

```rust
// src/emit.rs, already there:
pub const STACK_ID_ATTR: &str = "stack_id";  // Attr::uint
pub const STACK_ATTR: &str = "stack";        // Attr::text, root first, `;` separated
```

- Every event taken on a stack carries `Attr::uint("stack_id", id)`. The id is
  the identity `stack` aggregates by, and it is compared instead of the frames,
  so the same stack seen ten thousand times costs ten thousand `u64`
  comparisons and one string.
- The frames go on `Attr::text("stack", "main;work;read")`, root first, the
  same spelling `folded` uses. Putting it on *every* event is correct and
  putting it only on the first event bearing a given id is correct and cheaper:
  the table keeps the first it sees for each id and ignores the rest.
- An id whose frames never arrive is not dropped. It renders as
  `[unresolved stack 0x…]`, because a profile that silently omitted the samples
  it could not symbolize would understate exactly the part of the program the
  symbols were missing for.
- Events with no `stack_id` are counted in `Output::Stack::unattributed` and
  reported. `--emit stack` on a backend that captures none says so today, and
  `tests/aggregation.rs` asserts it — that test changes shape when stacks
  arrive and must not be left quietly passing.
- A kernel frame and a user frame are different things. Nothing in the emit
  layer inspects a frame's name, so B is free to mark them however it likes; if
  they should be visibly separated in the rendering rather than by their names,
  that is a request back to this layer and it is a small one.

`Results::stacks` is `BTreeMap<u64, Vec<String>>`, written by `emit::aggregate`
and only when the request asked for stacks.

### What would let `topk` answer the question `io` and `net` declare

`KeyValue::Dev { major, minor }` and `KeyValue::Cpu { cpu }` from stream D, and
the `io` and `power` collectors emitting one event per device or per cpu rather
than one summed event per tick. Nothing in `emit` changes. It is written up as
finding 2 and repeated here only because `topk` now makes the gap visible in
the output rather than in the model.
