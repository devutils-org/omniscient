# Open findings

Cross-cutting findings from the family threads, kept here because the notes they
came from were untracked and are gone. Each one is reproducible, names who it
belongs to, and is not a task list: several need a decision before they need
code.

---

## 1. The catalog declares per entry what actually varies per backend

*Closed for targets, closed at the model level for filter fields. What remains
is named in "Needs from another stream" below.*

Three threads hit this from three directions in one session, which is usually
the sign of a real modelling gap rather than three inconveniences.

`catalog::Backend` now carries its own optional `targets` and `filter_fields`.
Absent, a backend serves and carries whatever the entry declares, which is the
common case and keeps the files readable. Present, the map speaks only for that
backend and is not widened by the entry's: the entry describes the union of what
the chain can do, and the whole point is that a backend which only attributes
per cgroup must not be handed a system-wide request because a sibling could
serve one.

**Targets.** `io.bytes --cgroup <path>` resolved onto `/proc/diskstats` on this
machine -- the resolved record said so -- which is a backend that has no notion
of a cgroup. No wrong number reached anyone, because that collector happens to
check its own target and refuse at attach time, with exit 1 and "cannot start
collecting". Both halves of that are worth keeping in mind: the model made the
wrong choice, and what stopped it was one collector's defensiveness rather than
anything structural. A collector without that check answers with the machine.

The resolver now skips a backend that cannot attribute at the requested
granularity the way it already skips one whose requirements are missing, before
anything is attached, and the chain says what that backend would have answered
with instead:

```
fs:proc  rejected  `fs:proc` cannot attribute per cgroup: it would answer with
                   a number scoped to system
```

The request then lands on `fs:cgroup`, which this build has no collector for, so
it exits 3 -- "valid, and nothing is written for it" -- rather than 1. That is
the correct class: the request means something and this machine can serve it.

The loader refuses both ways the two maps can drift: a backend claiming a target
the entry refuses is unreachable, and a target the entry promises that no backend
serves is accepted and then resolves onto nothing.

**Filter fields.** Of the 42 sources with a collector, 26 declared a
`filter_fields` entry no collector in that family emits. Two different problems
were hiding in that one number, and they separate cleanly now.

Where a field is real through one mechanism and impossible through another, the
per-backend list says so and the resolver refuses the predicate rather than
running it against events that can never carry it. `comm` on `net.bytes` is the
case: the eBPF backend knows which process sent a byte and `/proc/net/dev` has
no idea. Before, that request succeeded and reported

```
filter `comm =~ 'omn'` kept 0 of 21 events, evaluated after reading
  no event carried `comm`, so that part of the predicate was never true
```

with exit 0, which reads as "nothing matched" and means "this could never have
matched". It is now refused by name, pointing at the backend that does carry the
field. Likewise `rwbs` and `comm` on the `io` diskstats sources.

Where a field is perfectly possible through the selected backend and the
collector simply does not emit it -- `comm` on the `pmu` counters, `pid` on
`sched`, `cpu` on `power` -- the catalog is not lying and nothing here can fix
it. That is collector work, and it is written up below with the signature that
would make it a failing test rather than a table in a document.

---

## 2. There is no key for a device or a cpu

*Closed.*

`KeyValue::Dev { major, minor }` and `KeyValue::Cpu { cpu }` exist, in the shape
`Iface` already had, with `KeyKind::Dev` and `KeyKind::Cpu` beside them. They
render as `dev:259+0` and `cpu:7`.

The catalog declares them where they belong: `dev` on the eight `io` entries
that are per device, `cpu` on every source that accepts a `--cpu` target. The
second is enforced rather than remembered -- a source that can be narrowed to a
core and cannot key a series by one could only ever answer with an average, so
a test sweeps the catalog for that combination.

They are declared and not yet produced. The collectors that need to emit them
are named below.

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

## 7. `sched.switches` fails its own ground truth about once in ten runs

Found while running the suite repeatedly for another change, and reproduced on
`1571a41` before any of it, so it is not a regression.

`collect::sched::switches::tests::the_switch_count_agrees_with_the_kernels_own`
compares what the perf sampler saw against the move in this process's
`voluntary_ctxt_switches` plus `nonvoluntary_ctxt_switches`. Ten runs of
`cargo test --lib` on the development machine produced one failure:

```
counted 21 switches against the kernel's own 40, with nothing reported as lost
```

The sampler's own count is stable at 21, which is the twenty sleeps the test
performs plus an edge. What moves is the reference: `/proc/[pid]/status` is
process-wide, and `cargo test` runs its four hundred tests in parallel threads
of that same process, so the counter is being driven by whatever else happens to
be running at that moment. The test's own slack (`measured * 10 >= kernel * 8`)
is a ratio, so a busier moment fails it.

This is the trap `docs/contributing.md` already names -- "anything measuring a
process-wide count cannot get an exact delta there; measure a launched process
instead" -- and the fix is the one that document recommends, in
`src/collect/sched/switches.rs`. It is not a loosened bound: a bound loose
enough to survive an arbitrarily busy process is loose enough for a collector
returning a constant to pass.

---

## Needs from another stream

### From whoever owns `src/collect/mod.rs` (stream F): a way to cross the catalog's claim against the build

The catalog now says, per backend, which filterable fields that backend carries.
Nothing checks that against what the collectors actually put on an event, so the
remainder of the 26 rows in finding 1 stays a table in a document that drifts.

Wanted, beside `implements`, which it mirrors exactly:

```rust
/// The attribute names a collector puts on every event it produces, so the
/// catalog's claim about a backend can be crossed against this build.
pub fn emitted_fields(source_id: &str, backend: &str) -> Option<&'static [&'static str]>;
```

`None` for a backend with no collector, which is the majority and is not a
failure. With it, one test over the whole catalog replaces the table: for every
implemented backend, every field the catalog says it carries has to be a field
the collector emits. `src/collect/gpu/device.rs` has the shape worth copying at
the family level, `every_filter_field_the_catalog_offers_is_on_the_events`.

### From the family threads of wave two: emit the two new keys

`KeyValue::Dev` and `KeyValue::Cpu` exist and nothing produces them yet.

- `src/collect/io/diskstats.rs` emits one event per tick carrying the sum over
  every leaf device, with a `devices` text attribute naming what was summed.
  One event per device carrying `KeyValue::dev(major, minor)` is what makes the
  `topk` over devices that `io.bytes` and `io.iops` declare expressible, and the
  major and minor are already parsed out of the file.
- `src/collect/power/{freq,cstate,throttle}.rs` mean across the watched cpus and
  report `min_hz`/`max_hz` beside the mean because they had nowhere to put a
  cpu. One event per cpu carrying `KeyValue::cpu(n)` replaces that.

Both are additive: the series key is an event's first key, so an existing
collector that adds a key at the end of its list changes nothing until it
chooses to lead with it.

### For the coordinator: `STATE.md` items 3 and 4 are done

"What remains, in the order it is worth doing" lists *Per-backend targets and
filter fields* third and *Keys for a device and a cpu* fourth. Both are closed
in the model, and the first is closed end to end: the resolver, the validator
and the loader all enforce it. The second is closed as vocabulary only -- the
variants and the catalog declarations exist, the collectors that produce them do
not -- so it moves rather than disappears, into the per-family work named above.

### From whoever owns `CATALOG.md`: §1 documents a schema that has grown

`CATALOG.md` §1 is the written schema for a catalog entry, and its example is
`io.latency` -- one of the entries this change touched. It now describes less
than the loader accepts. Two optional keys are missing from it, and the sentence
below the example needs one more line. Ready to apply:

```yaml
  - id: fs:proc
    requires: [/proc/diskstats]
    cost: free
    targets: {system: native}    # only where the chain disagrees
    filter_fields: [dev]         # only where the chain disagrees
    degraded: "aggregated per device, no per-request latency"
```

> `targets`: `native` (filtered at the backend level), `bpf_only` (filtered
> inside the program), `no` (invalid combination, must fail validation).
>
> A backend may carry its own `targets` and `filter_fields`. Absent, it serves
> and carries whatever the entry declares. Present, the map speaks only for that
> backend and is not widened by the entry's: the entry describes the union of
> what the chain can do, and a backend that only attributes per cgroup must not
> be handed a system-wide request because a sibling could serve one. The loader
> refuses a backend that claims a target or a field the entry does not offer,
> and an entry that offers one no backend in its chain serves.

### From the family threads: the fields a collector could emit and does not

These are the rows of finding 1 that the per-backend model does not reach,
because the mechanism can carry the field and the collector does not put it
there. Each is one attribute on an event.

| Family | Sources | Field |
|---|---|---|
| `cpu` | cycles, instructions, ipc, branch_misses, cache_misses, context_switches, migrations | `comm` |
| `mem` | rss, pss, uss, faults, heap.alloc | `comm` |
| `io` | bytes, iops, merges, inflight | `dev` |
| `sched` | wakeup, switches | `pid` |
| `power` | freq, cstate, throttle | `cpu` |
| `runtime` | jit | `comm` |

A predicate over one of these is accepted, runs, and matches nothing. The run
does say so -- "no event carried `cpu`, so that part of the predicate was never
true" -- which is why this is a gap rather than a lie, but it is a promise the
build does not keep.
