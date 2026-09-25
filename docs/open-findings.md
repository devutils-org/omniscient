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
- ~~**The GPU is not in the environment capture**~~ — it is, and has been since
  `f5ae1c6`. The resolved file carries every card the kernel describes, by PCI
  ids, driver and driver version, read from sysfs in 47 µs rather than from
  NVML: `{"card": "card1", "pci_id": "0x10de:0x1e07", "driver": "nvidia",
  "driver_version": "615.71.09"}`. What it deliberately does not carry is in
  `not_captured` beside it, vendor library versions included. This line stayed
  in two documents after the thing it describes was done, which is its own small
  lesson about where a claim lives. Not free:
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

## 7. What the join engine found outside itself

From the thread that implemented `eq`, `scoped_eq` and `wakeup`.

- **`--pid a,b` measures only `a`.** `Target::Pid` holds a list and every
  collector reads it through `Target::first_pid`, which returns the first and
  drops the rest silently: eleven call sites, every family. A run asking for two
  processes reports one, with no note anywhere that the second was ignored — and
  a join across two tasks, which is the case that distinguishes a correlator
  from a counter, cannot be set up through one session at all. Either the
  request refuses more than one pid until the collectors follow them, or
  `attach` takes the list. The join engine's own two-task ground truth had to
  run the tool four times and merge the streams to get around it.

- **A state that opened and was never ended was losing its attributions.**
  Found while generalizing, and it predates the generalization: opening a state
  on an identity that already had one overwrote it, so `opened` counted it and
  nothing else did. The engine now ends the state being replaced and counts it
  in `StateStats::replaced`, and `opened` now equals
  `closed + expired + overflowed + still_open` — an invariant the suite asserts
  on every correlation it builds.

- **`scoped_eq` has no producer yet.** Only `blk:rq@open_ts` carries a scope,
  and no collector emits a `Blk` key, so the match is exercised against
  constructed streams and the recycling it exists to prevent is demonstrated
  rather than observed. When stream E's kprobes land on the block layer, the
  first `io.latency` run is also the first real test of it: a completion whose
  `open_ts` does not match the open request must be counted, not attributed.

- **`examples/sched-wakeup.yml` can now declare its join.** It collects
  `sched.wakeup` and `sched.switches` side by side and correlates neither. The
  rule is three lines and belongs to whoever owns that file:

  ```yaml
  joins:
    - id: who-released-this
      from: sched.wakeup
      to: sched.switches
      via: task
      match: wakeup
      waker: waker_tid
      window: 1s
  ```

---

## 8. What the aggregation layer found once `hist`, `topk` and `stack` existed

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

Written down rather than fixed in passing, with the signature wanted.

### From stream A, in `src/encode.rs`: a correlation is not always a range

`correlation_text` renders every join as the allocation-fault one. It prints a
`heaviest ranges` heading, a `SIZE` column through `bytes(attribution.len)`, and
a closing paragraph explaining that the count is faults rather than bytes. Three
of the four matches have no extent, so `eq`, `scoped_eq` and `wakeup` all render
their attributions at `0 B` under a heading that calls them ranges, and a
wakeup chain is followed by a note about transparent huge pages.

`Attribution::key` already carries what each match means — `mem:[base,len)`,
`task:1234`, `blk:0xffff0000@99`, `task:99 -> task:100` — so what is wanted is
for the table to key off the match kind. `Correlation` does not carry it today;
it can, and the join thread will add it on request rather than guess the shape:

```rust
// in Correlation
pub match_kind: crate::catalog::MatchKind,
```

Say the word and it lands with the same defaults everything else has. It was not
added unasked because `Correlation` and `Attribution` are built as exhaustive
struct literals in `encode.rs`'s own tests, so a field added on this side breaks
the build on that one — which is also why `StateStats::replaced` could be added
(that literal uses `..Default::default()`) and nothing else could.

The same seam is why a `scoped_eq` run cannot yet report *why* a downstream
event went unattributed. "The pointer was reissued and this completion belongs
to the request before it" and "nothing ever opened that key" are different
facts, and only the second is a workload the reader can do anything about.

### From stream D, in `src/validate.rs`: refuse a malformed join before collecting

`check_joins` verifies that a rule's two sides are in the session and that both
carry the key. It cannot see the three ways a rule can fail against its own
match kind: an `interval` on a key that is not a range, a `scoped_eq` on a key
with no scope, and a `wakeup` that names no waker attribute or does not run on
`task`. All three are caught — but by the engine, which runs after the
measurement, so the cost of a malformed rule is the whole collection.

The engine exposes the check separately for this:

```rust
pub fn check(rule: &join::Rule) -> Result<(), join::JoinError>;
```

One call per rule in `check_joins`, pushing the error's own text as a
`Rejection`, and a session that cannot work is refused before anything attaches.

---

## 9. Thirty-six sources ask for tracefs where a raw tracepoint would do

Found while giving `sched.offcpu` a backend it could reach. The entry declared
`bpf:tp:stacks` and `bpf:tp`, both of which need a numeric event id out of
`/sys/kernel/tracing` — which is mounted and unreadable at uid 1000 on this
machine and, per `DECISIONS.md`, on most. So the source was unreachable *with*
`CAP_BPF`, and the resolver said `tracefs` three times.

That is not one entry's oversight. Across the catalog, **37 sources declare an
eBPF backend attached through a classic tracepoint and no raw-tracepoint
alternative**, and 36 of them name tracepoints this kernel publishes as
`btf_trace_<name>` in its own BTF — which is to say, as raw tracepoints, which
attach with a name and need no tracefs at all:

| Family | Sources |
|---|---|
| `io` | latency, queue, bytes, iops, size, merges, syscall, fs, pagecache, readahead, writeback |
| `mem` | faults, page_alloc, kmalloc, reclaim, compaction |
| `net` | tcp.lifetime, tcp.retransmit, drops, qdisc, softirq |
| `proc` | syscall, exec, exit, signals |
| `sched` | runqlat, switches, migrations, lock, futex, irq, numa_balance |
| `power` | freq, cstate, throttle |
| `cpu` | oncpu_time |
| `gpu` | job — the exception: `drm_run_job` and `drm_sched_process_job` are module tracepoints and are absent from `vmlinux` BTF, so they genuinely need the classic path |

`DECISIONS.md` already settled the rule — "`bpf:raw_tp` has strictly fewer
requirements than `bpf:tp` and is ordered ahead of it" — and one entry,
`sched.wakeup`, was written to it. The rest were written before it. Adding the
backend is data, not code: `{id: "bpf:raw_tp", requires: ["bpf", "btf",
"raw_tracepoint:<name>", "kernel>=6.1"], ...}` ahead of the `bpf:tp` entry,
with the same `degraded` note where the chain has one.

It was done here for `sched.offcpu` only, because without it the collector this
wave adds cannot be reached on the machine it was written on. The other
thirty-five are a catalog change nobody owns in this wave, and they are worth
doing in one pass rather than one at a time, because each one also needs its
`requires` checked against what the raw path actually needs.

**Wave three, stream A: the whole `sched` family is done** — `runqlat`,
`switches`, `migrations`, `lock`, `futex`, `irq` and `numa_balance` each carry
a `bpf:raw_tp` backend ahead of an `equivalent: true` classic one. The audit
was re-run against this kernel's BTF and is worth stating as a number, because
it settles how much of a mechanism the classic path still is: **thirty-nine of
the forty-one tracepoints the catalog names under `bpf:tp` publish a
`btf_trace_` type**, and the two that do not are the `gpu` pair this table
already calls out. So the premise that twenty-seven sources wait on a new
attach kind is not right: they wait on collectors. The mechanism they need is
the one `sched.wakeup` has used since wave two.

Reproducing the audit takes one command, and it is worth re-running per kernel
rather than trusted from here:

```console
$ comm -13 \
    <(strings -n 6 /sys/kernel/btf/vmlinux | sed -n 's/^btf_trace_//p' | sort -u) \
    <(grep -oh 'tracepoint:[a-z_0-9]*:[a-z_0-9]*' catalog/*.yaml |
      sed 's/^tracepoint:[a-z_0-9]*://' | sort -u)
drm_run_job
drm_sched_process_job
```

---

## 10. The sudoers line the Makefile documents cannot match, twice over

`make caps` runs `sudo -n setcap $(CAPS) $(BINARY)`, and the Makefile tells the
reader to permit exactly that with

```
<user> ALL=(root) NOPASSWD: /usr/sbin/setcap cap_bpf,cap_perfmon+ep /path/to/omniscient/target/debug/omn
```

Both halves fail on a real machine, and the failure is the one this project
minds most: `make` prints its "no capabilities" note, the suite goes green with
every eBPF test skipped, and nothing says the rule was never matched.

**The comma ends the command.** In `sudoers`, a comma separates entries in a
command list, so `cap_bpf,cap_perfmon+ep` is read as the end of one command and
the start of another -- and `cap_perfmon+ep` is not a path. `visudo` says
`expected a fully-qualified path name`, and a rule added without `visudo -c`
simply never matches. It has to be written `cap_bpf\,cap_perfmon+ep`.

**The path is relative.** `BINARY = target/$(PROFILE)/omn`, and `sudo` matches
an argument against the pattern literally rather than resolving it, so
`target/debug/omn` never matches `/path/to/.../target/debug/omn`. Permitting
the relative spelling instead would be worse than the problem: it would allow
`setcap cap_bpf` on whatever sits at `target/debug/omn` relative to *any*
directory the invoker chooses.

The fix is one word in the Makefile -- pass `$(abspath $(BINARY))` to `setcap`
-- and two characters in the documented line. Measured here: with the comma
escaped and the absolute path passed by hand, `sudo -n` succeeds and the
eBPF tests stop skipping; with the Makefile as it stands, they do not.

Worth doing with a glob, because `scripts/spawn.sh` creates a worktree per
stream and each has its own binary. A `sudoers` wildcard does not match `/`, so
one line covers them all and reaches nothing deeper:

```
<user> ALL=(root) NOPASSWD: /usr/bin/setcap cap_bpf\,cap_perfmon+ep /path/to/omniscient*/target/debug/omn
```

On a merged-`/usr` distribution `/usr/sbin` is a symlink to `/usr/bin` and both
spellings exist; `sudo` resolves the bare `setcap` through `secure_path` and
matches whichever it finds first, so both lines are needed.

## 11. Flaky ground-truth tests, and what each one is really measuring

Found by running `cargo test --workspace` a dozen times on a machine with other
builds on it, which is the condition none of them were written under. A test
that passes alone and fails under load is not noise: each of these compares two
readings taken at different instants, and load is what pulls the instants
apart.

One was in this stream's files and is fixed; two are not and are left alone.

**`sched::switches::the_switch_count_agrees_with_the_kernels_own` — fixed
here.** It failed about one run in four with `counted 21 switches against the
kernel's own 61, with nothing reported as lost`, which reads as the exact
defect the test exists to catch. It was the bracketing. `Collector::stop()`
disables the perf event and *then* drains it, and the drain allocates an
`Event` per switch, so the test read `/proc` after a drain that had itself been
preempted dozens of times with the event already closed. The two windows did
not coincide, and the gap between them grew with load. The test now disables
the event, reads the reference, and drains afterwards. Twelve workspace runs
since, no recurrence.

**`family_proc::a_count_without_its_limit_is_not_reported_as_a_measurement` —
belongs to `proc`.** It indexes `results[0].outputs[0].events[0]` and panics at
the subscript when the run produced no events. Under load the launched
`python3 -c 'import time; time.sleep(0.2)'` can take longer to start than the
window it is given, so there is nothing to index. The assertion it wants to
make is right; it needs to say "this run produced no events" rather than panic
on an index, because as it stands a failure blames the descriptor attributes
for a startup that was slow.

**`family_power::an_idle_share_stays_within_reach_of_the_window_it_covers` —
belongs to `power`.** Observed `idle_ratio=1.749` against a bound of 1.5, with
`idle_ns_total=8403946000` over an `interval_ns=200156095` across 24 cpus. This
is the late-crediting effect already recorded in finding 6: the kernel credits
residency when a cpu *leaves* an idle state, so a tick in which many long
sleeps ended is paid time belonging to earlier ticks. On a loaded machine more
cpus come out of C2 in one tick and the overshoot is larger. `1.5` was chosen
against an idle machine. Either the bound is a function of how many cpus can
wake in one interval, or the first tick of a run is excluded -- both are
decisions for whoever owns the source, and loosening the number without saying
which is happening would turn a real effect into a magic constant.

**Two more, in `gpu`, added later and from the same cause: the device is
shared.** `the_process_figure_and_the_device_figure_move_by_the_same_step` and
`a_resident_kernel_separates_from_an_idle_device_on_occupancy_and_on_power`
both failed in one workspace run and passed in the next. Measured at the time
of the failure, another process held 98 MiB of VRAM and the device read 8 %
busy: the first test allocates 256 MiB and requires the device figure to move by
the same step, which it cannot while a second allocator is moving it too, and
the second requires an idle device to read idle. Both are measuring the machine
rather than the tool -- the same class as finding 35, arriving through a device
instead of through a probe.

Neither is loosened, because the bound is not the problem: a GPU with another
tenant on it is a condition the scenario has to *detect* rather than tolerate,
the way the `power` scenarios skip loudly with the numbers that show why.
`device_to_ourselves()` now asks `nvidia-smi --query-compute-apps` before either
of them runs, and both skip naming the client and what it holds:

```console
skipped: another process is using this GPU, so a device figure is not this
         workload's: 4797, /home/moha/.local/zed.app/libexec/zed-editor, 98
```

That is the honest answer on the development machine, where the tenant is the
author's own editor and the condition is permanent rather than occasional. It
also means **these two scenarios do not run here**, which is a real loss and the
reason the skip names numbers: a reader who wants them exercised now knows
exactly what to close.

**One more, in `io`, and it is two clocks rather than a shared machine.**
`substituted_counters_are_converted_exactly_and_their_gaps_are_named` binds a
file it wrote over `/proc/diskstats` inside a namespace, then writes a known
increase into it on a schedule the test keeps. Its clock starts at `spawn`; the
run's starts once `unshare` has built a user and a mount namespace, bound the
file and exec'd `omn`. The lead-in between them was 250 ms, which is ample alone
and not ample under nine hundred concurrent tests -- and when it is not, the
increase the scenario chose becomes the baseline it is measured against, every
interval reads zero, and the failure blames the one piece of arithmetic the test
exists to check.

It went from occasional to reproducible when finding 36's catalog pass gave the
rest of the suite more work: twenty-four sources that used to stop at "no viable
backend" now resolve and run further. The lead-in is 700 ms now and the window
1500 ms, and where the race still wins the scenario says so and skips, because a
series with no movement at all cannot be a wrong conversion -- a wrong
conversion produces a wrong number, never an absent one.

## Needs from another stream

### From stream B (stack capture), for `sched.offcpu --emit stack`

`sched.offcpu` has a `bpf:tp:stacks` backend and this wave implements the one
without stacks. The kernel half of the missing part is small and belongs here:
a `BPF_MAP_TYPE_STACK_TRACE`, a `bpf_get_stackid` call, and the stack id
carried in the aggregation key instead of beside it — which is what makes a
profile of blocked time aggregate by stack rather than by task.

What is wanted from stream B is the half that turns ids into names, and the
seam has to be agreed before either side is written:

```rust
/// Frames of one captured stack, innermost first, as the kernel stored them.
/// A kernel stack and a user stack are different things and are asked for
/// separately: `sched.offcpu` wants both, and "this program is slow" and
/// "this program is waiting on the kernel" are different answers.
pub fn symbolize(pid: Option<i32>, addresses: &[u64]) -> Vec<Frame>;

pub struct Frame {
    pub address: u64,
    /// `None` when nothing could name it. A hexadecimal address is an honest
    /// frame; an invented name is not.
    pub symbol: Option<String>,
    pub offset: u64,
}
```

`bpf_get_stackid` returns a 32-bit id into a map whose values are arrays of
addresses, so this layer can hand over `(pid, &[u64])` and needs nothing else.
The identity and the side table stream A and B agreed on are downstream of
that, and this side does not need to know their shape.

### From stream A (aggregation), for `sched.offcpu --emit hist`

The catalog asks `sched.offcpu` for `hist`, and the entry is right to: a
distribution of blocked times is bimodal in exactly the cases worth finding,
and a total hides it. In-kernel aggregation as built here keeps
`(task, state) -> {total_ns, blocks}`, which is a total and a count and cannot
produce one.

The fix is in the key, not in the emit: add a log2 bucket to the aggregation
key, so the kernel counts `(task, state, bucket) -> blocks` and the histogram
falls out of the map. That is the bucket layout the brief already asks stream A
to use for `hist`, and this is the reason it matters — the layout an eBPF map
builds cheaply is the one the in-kernel path can produce at all.

It was not built in this wave because `hist` does not exist yet, so nothing
could check that the buckets are right. When it lands, the kernel-side change
is about twenty instructions and the key grows by four bytes.

### For whoever owns `catalog/*.yaml`

Two edits were made to `catalog/sched.yaml` from this stream, both under
`sched.offcpu`, because the alternative was a collector unreachable on the
machine it was written on:

- a `bpf:raw_tp` backend, between `bpf:tp:stacks` and `bpf:tp` (finding 7);
- two caveats: that preemption is excluded and belongs to `sched.runqlat`, and
  that a task still blocked when the run ends is in no total.

Neither touches `targets:`, `filter_fields:` or `keys:`, which is what stream D
owns in this file.

And one for stream D's own gap, finding 1: `sched.offcpu` declares
`system: native`, and every backend in its chain is eBPF. There is no native
path to a machine-wide off-cpu measurement and there cannot be one — `--system`
here means the eBPF program without a process filter, which is what the
collector does. `native` is the wrong word for it, and the same reading applies
to `sched.wakeup`.

---

## 12. What stack capture measured

From the stream that built `src/stack.rs`, on the development machine. Three of
these contradict something that was believed before it was measured.

- **A page-fault sample has no kernel half.** The kernel records a software
  page-fault event against the registers that took the fault, which for a fault
  from user code are user registers, so `get_perf_callchain` skips the kernel
  walk entirely: 64 fault samples, every one of them `PERF_RECORD_MISC_USER`,
  zero kernel frames. A context switch is the opposite -- taken in the scheduler,
  in kernel mode -- and its stack crosses the boundary every time. A test for the
  kernel half has to use an event that fires in the kernel, which the fault
  sampler is not.
- **A target built without frame pointers does not yield one frame.** It yields
  a short, plausible, wrong one. The same fixture built twice gave
  `_start;__libc_start_main;lang_start;omn_fixture_a;omn_fixture_b;omn_fixture_c`
  with frame pointers and `_start;__libc_start_main;lang_start;omn_fixture_c`
  without: the walk followed whatever `%rbp` held, which was the frame the C
  runtime last set up, and came back naming three callers that did not call
  anything. Nothing in the addresses says so.
- **The prologue says so.** `push %rbp; mov %rsp,%rbp` -- behind an `endbr64`
  where indirect-branch tracking is on -- is four bytes read from the file the
  leaf frame was sampled in, and it turns "the profile may be wrong" into a
  count. 199 of 199 samples of the no-frame-pointer build landed in a function
  that sets none up; none of the other build's did. This is what
  `stack::Unwinder::notes` reports, and it is a measurement rather than a
  suspicion.
- **A Rust debug build omits frame pointers.** `-C opt-level=0` alone does not
  keep them on `x86_64-unknown-linux-gnu`; `-C force-frame-pointers=yes` does.
  Any fixture whose point is a call chain has to ask for them explicitly, or it
  is testing this repository's profile settings.
- **An unoptimised `for index in 0..n` is three calls per turn.** A profile of a
  fixture that used one landed in `Iterator::next`, `RangeIteratorImpl::spec_next`
  and `Step::forward_unchecked` more often than in the function under it. A
  `while` loop over operators put 199 of 199 samples where the fixture says the
  time goes.
- **Symbols are not demangled.** `chain::main` appears as
  `_RNvCsezHSzhuRag8_5chain4main`. Rust has two mangling schemes and C++ has
  another, and half a demangler produces confident nonsense on the half it does
  not know, so the symbolizer reports the linker's name. Worth doing properly,
  and it belongs with the symbolizer rather than with any renderer.

---

## 13. `sched.switches` fails its own ground truth about once in ten runs

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

### From stream B (stack capture), to stream A (aggregation and presentation)

The seam the brief asked the two of us to agree on, now built on this side. A
captured stack is an attribute on the event plus a side table, and `Event` is
untouched.

- The event carries `stack::STACK_ID_ATTR` (`"stack_id"`), an
  `AttrValue::Uint`. The value indexes the table below and means nothing without
  it.
- `emit::Results` wants the table beside the outputs:

  ```rust
  /// The stacks the events of this run refer to by id. Empty unless something
  /// captured any.
  #[serde(default, skip_serializing_if = "StackTable::is_empty")]
  pub stacks: crate::stack::StackTable,
  ```

  `StackTable` is `Clone + Debug + PartialEq + Serialize + Deserialize` and
  serializes as a plain array of stacks, so a result file gains one field and no
  new shape. `table.get(StackId)` is the lookup, `table.iter()` yields
  `(StackId, &Stack)`.
- `--emit stack` aggregates by id, never by frame list. The suggested shape, for
  stream A to accept or replace:

  ```rust
  Output::Stack { profile: Vec<StackCount> }
  pub struct StackCount { pub stack: StackId, pub samples: u64, pub value: f64 }
  ```

  Sorted descending by `value`, which is what every consumer of a profile wants
  and what `topk` over stacks already means.
- The `folded` encoder needs no stack logic: `stack::Stack` implements `Display`
  as `main;work;read`, root first and semicolon separated, which is the whole of
  that format's left-hand side. `format!("{stack} {count}")` per line.
- `stack::Unwinder::notes()` returns the degradations of a capture, in the
  wording `Collection::notes` already uses. They reach `Results::degradations`
  through the collector, so nothing in `emit` has to know about them.

### From stream B, to stream D (the catalog model)

`validate::check_stack` warns that `--stack dwarf` is expensive. It is not
expensive; it does not exist. `stack::callchain_for` refuses `dwarf` and `lbr`
by name, and the validator should refuse them too rather than let a request get
as far as a collector to be told there:

```rust
// in check_stack, replacing the `dwarf` cost note
if let Err(reason) = crate::stack::callchain_for(stack) {
    rejections.push(Rejection::new(reason));
    return;
}
```

The same function is the resolver's answer to "which unwind mode when none was
given": `StackMode::Fp` where the request emits stacks, `StackMode::None`
otherwise.

### From stream B, to whoever writes the first profiling collector

`src/stack.rs` and the callchain half of `src/perf.rs` are the capture layer and
nothing calls them yet: this wave gave `src/collect/cpu/**` to no stream. A
collector that wants stacks does four things, and `tests/stack_capture.rs` does
all four:

```rust
let mut event = SampledEvent::open_software_with(
    config, period, scope, ring_pages, stack::callchain_for(mode)?,
)?;
let mut unwinder = stack::Unwinder::for_process(pid);   // while the target lives
// per sample, during the drain:
let id = unwinder.record(&sample);                      // -> Option<StackId>
// at the end: unwinder.notes() into `Collection::notes`, unwinder.into_table()
// into the results.
```

The ordering constraint is the one that is easy to get wrong: symbolization
reads `/proc/<pid>/maps`, which is gone the moment the target exits, so stacks
must be interned as the ring is drained and not once at the end of the run.

---

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

---

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
---

## 14. Granting capabilities removes three sources that needed none

Found while verifying the build after `setcap`, by running the same binary
twice.

```console
$ getcap target/debug/omn
target/debug/omn cap_perfmon,cap_bpf=ep
$ omn mem.heap.retained --mode poll@100ms --emit timeline -- python3 …
error: `mem.heap.retained` has no viable backend on this machine:
  preload: /proc/[pid]/pagemap: missing: /proc/1659117/pagemap exists but is not
  readable by uid 1000

$ cp target/debug/omn target/debug/omn-nocaps     # byte-identical, no capabilities
$ omn-nocaps mem.heap.retained --mode poll@100ms --emit timeline -- python3 …
mem.heap.retained  via preload  (mem.heap.retained)
  1 events over 33.04ms, stopped because the launched program exited …
```

The pid in the refusal is `omn`'s own. A binary that gains privileges at exec
is marked non-dumpable, and the kernel reassigns its `/proc/<pid>` entries to
root; `mem.heap.alloc`, `.live` and `.retained` read `pagemap` and
`smaps_rollup` of the process holding the shim's ring, so they get EACCES on
files belonging to the user who started the run.

So the fix for one silent failure produced another of the opposite sign.
`make` reapplies the capabilities because a build without them under-reports
what the machine can serve; the capabilities then under-report it again, in the
other three sources, and the two errors do not cancel — they read as a machine
that changed.

**Fixed** in `main.rs`: `prctl(PR_SET_DUMPABLE, 1)` at startup, which is the
same call `collect/launch.rs` already makes in every launched child and for the
same reason. It gives nothing away: `ptrace` of a task holding capabilities the
tracer lacks is refused by the capability subset check, a separate barrier that
still stands.

The regression test does not need the capabilities to run, because
`PR_SET_DUMPABLE` reaches the same state by another route — an ordinary process
that sets `dumpable = 0` also loses the ability to read its own `pagemap`. It
runs in a forked child, since dumpability is process-wide and the rest of the
suite is in the other threads.

**Generalisation worth carrying**: any collector that reads `/proc/self/...`
rather than a target's is measuring a process whose procfs ownership depends on
how the binary was installed. There are no others today; `capabilities.rs`
substitutes `[pid]` with `self` when a requirement names no target, and that is
exactly where this surfaced.

---

## 15. A `bpf:tp` requirement is reported available when only its directory is

From stream A, wave three. This is finding 9 seen from the other side, and it
is the sharper half: the catalog is not the only place the classic tracepoint
path is wrong about itself.

`capabilities.rs` answers `tracepoint:<category>:<name>` by probing the event's
**directory** under tracefs:

```rust
probe_path(&root.join("events").join(event.replace(':', "/")))
```

On this machine `/sys/kernel/tracing` is `drwx---r-x root root`, so uid 1000
traverses and lists it freely, while the file the attach actually needs is
`-r--r----- root root`:

```console
$ ls -ld /sys/kernel/tracing/events/sched/sched_switch
drwxr-xr-x 2 root root 0 ...                      # listable by anyone
$ cat /sys/kernel/tracing/events/sched/sched_switch/id
cat: .../id: Permission denied
```

So the probe says **available**, the resolver selects the backend, the run
launches its target, attaches its other collectors, and only then discovers it
cannot read the number. That is precisely the trap `BRIEF.md` names for this
stream: *"a `bpf:tp` backend that needs a file this user cannot read must say so
as a missing requirement, not fail at attach time."*

Reproduced now, with the capabilities granted:

```console
$ omn list sched.lock
    2. bpf:tp                 cost med    no collector yet
       needs    tracepoint:lock:contention_begin       available
       needs    tracepoint:lock:contention_end         available
```

Neither is available. Nothing on this machine can read either id.

One consequence is already in the tree and is not a defect: `sched.offcpu`
orders `bpf:tp:stacks` ahead of `bpf:raw_tp`, correctly, because it offers the
kernel side of a blocking stack that the raw path gives up. That backend has no
collector today, so the resolver falls through and the run works. **The day
someone writes it, it will be selected on this machine and fail at attach**,
because its `tracepoint:sched:sched_switch` requirement probes as available.
The ordering is right; the probe is what is wrong.

### Needs from another stream: the probe `src/capabilities.rs` should call

Written and tested in `src/bpf/attach.rs`, which is stream A's file; the probe
that would use it is not.

```rust
/// Whether the classic path can attach to `<category>:<name>` here.
pub fn classic_tracepoint_support(event: &str) -> crate::bpf::Support;
```

It reads the `id` file the attach reads, and distinguishes the three cases that
call for different answers: absent (a different kernel), unreadable (root or
`CAP_DAC_READ_SEARCH`, which is more than this project asks for), and tracefs
not mounted. The change in `capabilities.rs` is one arm:

```rust
if let Some(event) = requirement.strip_prefix("tracepoint:") {
    return Probe::from(crate::bpf::attach::classic_tracepoint_support(event));
}
```

The same file has no probe at all for `kprobe` and `uprobe`, which
`sched.lock` and `sched.mutex` declare and which therefore read `unverified`.
`crate::bpf::attach::probe_type_support(kind)` answers both, by reading the PMU
type the kernel publishes at boot.

---

## 16. Every `bpf` backend is credited with a filter it does not apply

From stream A, wave three. `resolve::placement` decides where a predicate runs
from the backend's **name**:

```rust
fn placement(backend: &Backend, entry: &Entry) -> FilterPlacement {
    if backend.id.starts_with("bpf") {
        FilterPlacement::Kernel(...)
```

and `collect::apply_filter` then leaves those events alone, deliberately and
with a comment saying why: *"running the filter twice would hide the bug if it
had not."* The bug it was guarding against is the one that is there. No eBPF
collector in the tree applies a predicate, so the filter is applied by nothing:

```console
$ omn sched.wakeup --system --duration 1s --emit summary \
      --filter "comm == 'definitely_not_a_comm'"
  1341 events over 1.01s
```

A predicate that matches nothing kept everything, and the run said `no loss`
and reported no filtering at all. `sched.offcpu` behaves the same way.

This is not a small wrongness. A filter is how someone narrows a machine-wide
trace to the thing they are investigating, and a filter that silently does
nothing answers a different question under the name of the one asked.

`sched.runqlat` applies its own predicate over the drained rows and says so in
the run's output — *"the filter kept 417 waits and removed 2922; it ran here,
over the drained rows, not in the kernel"*. That makes the numbers right and
leaves the resolver's claim about the *cost* untrue.

### Needs from whoever owns `src/resolve.rs` and `src/collect/mod.rs`

The credit should come from the collector, not from the backend id. Two shapes,
either of which closes it:

1. `Collector` grows `fn filters_in_kernel(&self) -> bool { false }`, and
   `apply_filter` runs the predicate unless the collector claims it. A
   collector that pushes the predicate down opts out; every other one is
   correct by default, which is the right direction for a default to point.
2. `placement` consults the registry rather than the name. Harder, because
   placement is decided before anything is built.

Until then, an eBPF collector that does not apply its request's predicate is
returning events the user asked to be rid of, and the resolved file says the
kernel removed them.

---

## 17. The emit layer cannot weigh an event, so a kernel-built histogram renders wrong

From stream A, wave three, and it is the one thing `sched.runqlat` gives up.

`BucketLayout::Log2`'s own doc comment anticipates this: *"this is the layout an
eBPF array map builds for free ... which is what makes moving a histogram into
the kernel viable later."* The layout does; the consumer does not. `emit::hist`
counts one per event:

```rust
*counts.entry(layout.index(value)).or_insert(0) += 1;
```

A source that bucketed in the kernel drains `(key, bucket) -> count` and has to
present each row as one event. The buckets come out in the right places — that
is checked end to end in `tests/family_sched.rs` — and each is one unit high
instead of `count`.

### Needs from stream D, or whoever owns `src/emit.rs`

A reserved attribute, exactly like `STACK_ID_ATTR` already is:

```rust
/// How many measurements this event stands for. Absent means one.
pub const WEIGHT_ATTR: &str = "count";
```

honoured by `hist` (add `weight` rather than 1 to the bucket), `summary`
(`count`, `sum`, and therefore `mean`), `rate` and `topk`. `min` and `max` are
already right, since every measurement behind a row is inside its bucket.

`sched.runqlat` emits `count` on every event today, so the day this lands the
source is correct with no change on its side. Until then the run says what it
is giving up, in the words above — but a `degraded:` line is a weaker thing
than a right number.

Two sources already want this and a third will: `sched.runqlat`,
`sched.migrations` (whose `migrations` attribute is a count over an interval),
and any `--emit hist` that a budget moves into the kernel, which is stream E's
whole premise.

---

## 18. Two claims the tool makes about kernel-side histograms are now false

Both are one line each, and both are in files stream A does not own.

**The validator's warning.** `omn sched.runqlat --emit hist` prints:

> `trace` with `hist` aggregates in userspace unless a budget lets the resolver
> move the histogram into the kernel; consider --budget

The histogram *is* in the kernel, unconditionally, and no budget is involved.
The warning should be conditioned on the selected backend rather than on the
pair `(trace, hist)`.

**`docs/bpf-layer.md`'s "What is not here yet"** lists *"a log2 bucket in an
aggregation key ... about twenty more instructions"*. It is there now, in
`src/bpf/runqlat.rs`, as an unrolled six-step binary search — thirty
instructions, no loop, and `LOG2_STEPS` is the table. The entry should point at
it rather than describe it as future work. The same section's first bullet,
*"kprobes, uprobes, fentry and USDT: only raw tracepoints attach today"*, is
half done: `src/bpf/attach.rs` has the first three attach kinds, and fentry and
USDT are still absent.

---

## 19. Symbolising a machine-wide run costs two minutes per second observed

From stream A, wave three, found while fixing the half of it that was waste.

`sched.offcpu --system --duration 1s --emit summary` did not finish in ninety
seconds. Half the cause was a defect in `src/collect/sched/offcpu.rs` and is
**fixed**: the collector symbolised every stack it drained whether or not the
request asked for frames, and the emit layer discards the table unless `stack`
was asked for. That run now takes 1.2 seconds.

What is left is not waste and is not fixed. The same run with `--emit stack`
takes **1 minute 57 seconds** for one second of observation, all of it in
`src/stack.rs`: a machine-wide run meets every process on the machine, and each
one costs a read of `/proc/<pid>/maps` and then of its binaries' symbol tables.

That is roughly 0.4 seconds per process, and it is worth someone's attention
because the ratio is what makes a source unusable rather than slow. Three
things that would each help, in the order they look cheapest:

- **Cache by binary, not by process.** Every process on a machine maps the same
  handful of libraries; the symbol table of `libc.so.6` is read once per
  process today.
- **Resolve only the ids that survive.** A machine-wide run drains thousands of
  rows and a reader looks at the top few; resolving is currently eager because
  the maps disappear when the process does, which is real but applies to the
  launched target rather than to every process on the machine.
- **Bound it, and say so.** A run that symbolised four hundred processes and
  gave up on the rest is a degradation with a name, which is this project's
  usual answer.

---

## 20. `comm` is the name a task had when its row was created

From stream A, wave three. `sched.offcpu`, `sched.runqlat` and
`sched.migrations` all read `task_struct.comm` once, when a row is first put in
the map, rather than on every event — deliberately, because that path runs tens
of thousands of times a second and a name does not change.

Except at `exec`, which is exactly the moment a launched target acquires the
name anyone would recognise. A short run of a launched program reports the
first rows as `comm=omn`:

```console
$ omn sched.runqlat --emit raw -- /bin/sh -c 'sleep 0.02'
    ... wait_ns=4096 count=2 ... comm=omn cpu=1 prio=120
```

The child is held at the launch gate before its first instruction, so it is
still a fork of `omn` when it first joins a run queue. Rows are drained and
recreated every interval, so the name corrects itself within one tick — which
is why this is a note rather than a defect, and why a run shorter than a tick
gets it wrong for every row it produces.

Three ways out, none obviously right: read `comm` on every event and pay for
it; re-read it on the drain, from `/proc/<tid>/comm`, which is a different
process's file and may be gone; or attach `sched_process_exec` and correct the
rows it names. The third is the one that is actually correct and it costs a
fourth program.

---

## 21. A requirement can only be probed by reading, and some are written

Found by stream B (memory breadth) while landing `mem.wss`.

`capabilities.rs` answers a requirement that starts with `/` by calling
`access(R_OK)` on the path, and reports three outcomes: readable, restricted,
absent. That covers every requirement in the catalog but one, and the exception
is not exotic — it is how half of procfs's *controls* work.

`/proc/[pid]/clear_refs` is mode `--w-------`. It is the file a process writes
to reset the kernel's referenced bits, which is the only unprivileged way to
measure a working set, and it is writable by the owner of the process and by
nobody else. Probed by reading, it comes back as

```
/proc/2517642/clear_refs exists but is not readable by uid 1000:
reading it needs root, or a mode that lets this user read the file
```

which is true, useless, and exactly backwards: the file is usable and the probe
says it is out of reach. `mem.wss` resolved past its only implemented backend
onto `fs:sys:damon` and the run failed with "no collector is written".

**What is needed**, from whoever owns `src/capabilities.rs`: a requirement
prefix that asks the other question.

```rust
// in Capabilities::probe, beside the `/` branch
if let Some(path) = requirement.strip_prefix("writable:") {
    return probe_writable(&substitute_pid(path, pid));
}
```

with `probe_writable` calling `access(W_OK)` and keeping the same three
outcomes — `Available`, `Missing("… exists but is not writable by uid N")`,
`Missing("… does not exist")`. One branch and one function, and the `[pid]`
substitution it needs already exists.

Until then `mem.wss`'s `fs:proc:clear_refs` backend declares only
`/proc/[pid]/smaps_rollup`, which is the file it *reads*, and checks the write
at attach time with a refusal that names the file and the reason. That works
and is honest at the point of use, but it is a requirement the catalog does not
state, so `omn list mem` cannot tell a user whose process it is from a user
whose process it is not. Two other sources will want the same prefix the moment
they are written: `mem.wss` through `page_idle` writes the idle bitmap, and any
soft-dirty tracker writes the same `clear_refs`.

## 22. A NUMA node has no key, so per-node series cannot be told apart

Found by stream B while landing `mem.numa`.

`KeyValue` is a closed set, deliberately, and it has no variant for a memory
node. `mem.numa` through `/proc/[pid]/numa_maps` produces one event per node per
tick — which is the right shape, because a process with two thirds of its pages
on node 0 may be doing all of its work from node 1, and one total would hide
exactly that — but every one of those events has `task` as its primary key, so
`SeriesKey::of` folds them into a single series and a `timeline` interleaves the
nodes.

The node is on the event as an attribute and is a declared filter field, so
`--filter node==1` recovers one node's series. That is a workaround, not the
model: the same request over two nodes should be two series, the way two devices
are two series for `io.bytes`.

**What is needed**, from whoever owns `src/event.rs`:

```rust
/// A memory node, as the kernel numbers them.
Node { node: u32 },
```

plus `KeyValue::node(node)` and a `Display` of `node:N`, and the `node` entry in
whatever `KeyKind` enumerates. `mem.numa` would then put it first on each event
and the emit layer needs no change at all. `power` and `cpu` will want it too:
per-node uncore and per-node memory bandwidth have the same shape, and
`mem.bandwidth`'s uncore backend is package-scoped for the same reason.

This is the same gap `power.cstate` has with `state`, named in `STATE.md`: a
source that produces one measurement per member of a set the key model does not
know about.

## 23. The PSI reader now exists twice, in two families

`io.pressure` and `mem.pressure` read the same file format — `some`/`full`,
three decayed averages and a microsecond counter — and there are now two
readers of it, `src/collect/io/pressure.rs` and `src/collect/mem/pressure.rs`.
The second was written rather than shared because the wave's rules give each
stream only its own family's directory, and the honest alternative to
duplicating was to leave `mem.pressure` unwritten.

They agree today, deliberately: the same field names on the events
(`stall_ratio`, `full_stall_ratio`, `stalled_ns`, `some_avg10`…), the same
first-read-is-a-baseline rule, the same "a snapshot reports the kernel's
averages and says so". That agreement is a convention nothing enforces, and
`cpu.pressure` makes three — `irq.pressure` would make four, and the kernel
publishes it.

**What is needed**, from whoever owns `src/collect/mod.rs`: a `psi` module
beside `task.rs`, which is the precedent — `comm` was read by each family
separately until it was not. The shape is already settled by the two copies:

```rust
pub struct Psi { /* avg10, avg60, avg300, total_us */ }
pub struct Reading { pub ts_ns: u64, pub some: Psi, pub full: Option<Psi> }
impl Reading {
    pub fn parse(text: &str, ts_ns: u64) -> Option<Self>;
    pub fn average_attrs(&self) -> Vec<Attr>;
}
```

The `full` line stays optional there for the reason both copies already give:
a cgroup on an older kernel writes no `full` line, and zero would claim nothing
was ever fully stalled.

Not urgent, and worth doing before the fourth copy rather than after.

---

## 24. A session polls every collector at the pace of its fastest one

Found by stream C while writing `examples/cpu-per-core.yml`. It belongs to
whoever owns `src/collect/mod.rs`.

`run_all` picks one interval for the whole run:

```rust
let interval = collectors.iter().filter_map(|c| c.tick()).min().unwrap_or(…);
```

The comment above it is right about why -- "the fastest collector sets the pace:
draining a ring every second loses events, and the cheap pollers do not mind
being asked more often" -- but a poller does mind, for a reason that has nothing
to do with cost. A counting collector returns `Some(10ms)` from `tick`, not
because it wants to be read that often (it has nothing to give until the window
closes) but so the run notices a target that exited. Any polled source sharing
that session is then asked every 10 ms, whatever its request said.

Measured, on this machine:

```console
$ omn cpu.util --cpu 0-3 -m poll@500ms -e timeline -d 3s
  24 events over 3.00s          # six points per cpu, as asked
    cpu:0  +0ns 0.078  +500ms 0.042  +1s 0.118  +1500ms 0.040  …

$ omn -f examples/cpu-per-core.yml          # the same request beside two counters
  1160 events over 3.01s        # two hundred and ninety points per cpu
    cpu:0  +0ns 0.000  +10ms 0.000  +20ms 1.000  +30ms 0.000  …
```

Two things go wrong, and the second is the one that matters. The series has
fifty times the points the request asked for, which is only noise. And each
point is a share computed over one tick of the kernel's own accounting, so it
can only be 0 or 1: the file credits a whole tick to whatever was running when
the timer fired. A utilisation that reads `0.000, 0.000, 1.000, 0.000` is not a
noisy version of `0.042` -- it is a different quantity, and its mean is only
accidentally close.

The same applies to `cpu.oncpu_time`, which says so on the event: it publishes
`resolution_ns` and notes when the interval is close to a tick. A collector
being polled faster than it was asked to be cannot emit that warning, because it
does not know the interval changed.

Two shapes of fix, and the choice is a decision rather than a patch:

- **Per-collector cadence.** Each collector is asked at `max(its own tick, the
  interval its request named)`. The runner already knows both. A ring still
  drains at its own pace because its `tick` is the floor, not the ceiling.
- **Separate "how often it must be drained" from "how often it has a value".**
  `tick` means the first today and is being read as the second. A counting
  collector wants to be *looked at* every 10 ms and has a value once.

Stream C's example works around it by keeping the polled source out of the
session and saying so in a comment, which is not a fix.

---

## 25. A `pmu:` requirement cannot be a glob, and a qualified one answers vaguely

Two small things in `src/capabilities.rs`'s `probe_pmu`, found by stream C while
making `cpu.topdown` collectable.

**The glob.** `cpu.topdown` required `pmu:topdown-*`. Nothing expands it: the
probe looks for a performance unit or an event called literally `topdown-*`,
does not find one, and reports it missing -- which happened to be the right
answer on this machine for the wrong reason, and would have been the wrong
answer on an Icelake. Fixed in the entry by naming the four events, which is
also what the collector opens.

**Two more globs are still in the catalog, and they are reporting the wrong
answer right now.** `io.fs` requires `tracepoint:ext4:*` and `io.readahead`
requires `tracepoint:filemap:*`. That probe class checks a path under tracefs,
so it checks a directory literally named `*`:

```console
$ omn io.readahead --system -m count -e summary -d 100ms
error: `io.readahead` has no viable backend on this machine:
  bpf:tp: tracepoint:filemap:*: missing:
          /sys/kernel/tracing/events/filemap/* does not exist

$ ls /sys/kernel/tracing/events/filemap/
file_check_and_advance_wb_err   mm_filemap_add_to_page_cache
filemap_set_wb_err              mm_filemap_delete_from_page_cache
mm_filemap_fault                mm_filemap_get_pages
mm_filemap_map_pages
```

Seven tracepoints, and the source reports the requirement missing by name. This
is a false negative in the one table the tool exists to be trusted about, and it
belongs to whoever owns the `io` entries -- it is fixed the same way
`cpu.topdown` was, by naming the events the collector will actually attach to,
since "any of them" is not a requirement a chain can be resolved against. A
loader assertion refusing `*` in a requirement would keep all three from coming
back, for a character that looks harmless in YAML.

**The qualified form.** `probe_pmu` handles a bare name (`slots`) by scanning
every unit's `events` directory, and a qualified one (`cpu/slots`) by reading
that unit's files. When the qualified form finds nothing it falls through to
`sysfs_device_state`, which returns `None` for any name containing `/`, so the
answer is `Unknown`: "`pmu:cpu/slots` needs an event encoding this tool does not
build yet". The truth is narrower and more useful -- this machine publishes no
such event under that unit -- and the distinction is exactly the one the probe
vocabulary exists to make, since `unknown` never counts as available while
`missing` says whose problem it is. The bare form is used throughout the catalog
and gives the right answer, so nothing is broken today.

---

## 26. Two readers of `/sys/devices/system/cpu/online`, in two families

`power::sysfs::online_cpus` has parsed the kernel's cpu list since the first
wave, and it is `mod sysfs` -- private to `src/collect/power/`. Stream C needed
the same list to open one counter per cpu and could not reach it, so
`perf::online_cpus` and `perf::parse_cpu_list` now exist beside it, with the
same handling of holes in the numbering.

Two copies of eight lines is not expensive; two copies that drift are. The
question this poses is where it belongs: reading which cpus exist is not a
`power` concern nor a `perf` one, and at least three of the streams in this wave
want it. `src/capabilities.rs` already answers "what does this machine have" for
everything else and is the obvious home, with both families calling it. Left
alone here rather than moved, because moving it means editing a file stream C
does not own -- but the next stream that needs a third copy should move it
instead of writing one.

---

## 27. What the network breadth stream measured

Landed by the stream that owns `src/collect/net/**`, building `net.capture`
over `AF_PACKET` and `net.hwts` over the `ethtool` netlink family and the PTP
character device. Four things here are worth knowing outside that directory.

**A capture's privacy boundary belongs in the kernel, not in the code.** The
obvious way to keep a payload out of the output is to parse the headers and
print only those. The output is then clean and the payload is still in this
process's address space, in a buffer shared with the kernel, for as long as the
ring holds the block. So the boundary is placed one layer down instead: a
classic BPF filter of a single instruction, `BPF_RET | BPF_K` returning 96,
tells the kernel how many bytes of each frame to copy. Ninety-six is an
Ethernet header, the largest IPv6 header the reader walks and a TCP header. The
payload never crosses. `tp_len` still reports the frame's real length, so
nothing about the measurement is lost:

```console
$ omn net.capture --iface lo --mode trace --duration 2s --emit raw --format json
  bytes 142   captured_bytes 96   protocol udp   dport 39999   direction outgoing
```

142 is 14 + 20 + 8 + 100 for a hundred-byte datagram, measured; 96 is the cap.
The same one-instruction program is where a pushed-down predicate would go, and
that is the subject of a request below.

**Loopback carries every frame twice, and `af_packet` shows it as two frames
rather than as one counted twice.** `net.bytes` has carried the caveat "loopback
traffic is counted twice, once received and once transmitted, because it is
both" since the first wave; at frame level the two are distinguishable, by the
`sll_pkttype` the kernel attaches. A hundred datagrams produce exactly a hundred
frames with `direction outgoing` and exactly a hundred with `direction host`.
That is what makes the capture's ground truth an equality rather than a range.

**`CLOCK_REALTIME` is slew-bounded and the bound is usable.** A packet socket
stamps frames from the realtime clock, so every timestamp has to be converted,
and the conversion has to say how wrong it may be. Two terms: the width of the
calibration sandwich (monotonic, realtime, monotonic — about 11 µs on this
machine, and dominated by the two monotonic reads), and drift since. The second
is bounded because `MAXFREQ` in `kernel/time/ntp.c` caps what `adjtimex` will
slew to 500 parts per million, so an offset measured one 20 ms drain ago is
wrong by at most 10 µs. What no bound covers is a *step*: `settimeofday` moves
the clock by any amount at any moment. That cannot be predicted and it can be
caught, because two calibrations that disagree by more than the slew allows can
only have had a step between them. `foreign_clock.rs` checks for it and widens
the error of everything converted afterwards rather than shifting a window of
timestamps silently.

**A machine's PTP hardware clocks are readable without privileges, and they are
a real second clock domain.** On this machine `/dev/ptp0` and `/dev/ptp1` are
mode 0664 root:clock, so `clock_gettime` on the descriptor works as an ordinary
user. `enp38s0` names clock 0 and reports an offset of about
1.79 × 10¹⁸ ns from `CLOCK_MONOTONIC` — which is the gap between the 1970 epoch
and this machine's boot, so that clock is disciplined to realtime. Polled, the
offset moves by far less than one part in a hundred of the window, which is the
ground truth both the unit test and the integration test use: two independent
oscillators agreeing about the passage of a known interval is a check that owes
nothing to the code that reads them.

**And what is not there.** No interface on this machine is *configured* for
hardware timestamping; `ETHTOOL_MSG_TSINFO_GET` reports what a card can do, and
turning it on is a system-wide change with side effects for anything else using
the NIC, which this tool does not make. So `net.hwts` says where the card's
clock is, not that any packet was stamped with it.

---

## 28. A cost band is declared per backend, and what a run costs is not

The first thing the measurement found, and it is a modelling gap rather than a
bug. `catalog::Cost` is a property of a backend: `fs:proc:smaps_rollup` declares
`free`, `fs:proc:smaps` declares `low`. What a run actually costs is that
backend's price *per read*, multiplied by how often the request asks for one --
and the request is where the rate lives.

The same source, the same backend, the same program, three poll intervals:

```
$ omn mem.rss -e summary -m poll@200ms -- sh -c '<cpu-bound loop>'
cost: mem.rss (poll@200ms) via `fs:proc:smaps_rollup` spent 1.6ms of cpu
      against the target's 996.7ms (0.16 %, declared `free`)

$ omn mem.rss -e summary -m poll@10ms -- sh -c '<the same loop>'
cost: mem.rss (poll@10ms) via `fs:proc:smaps_rollup` spent 5.1ms of cpu
      against the target's 272.1ms (1.86 %, declared `free`)

$ omn mem.rss -e summary -m poll@1ms -- sh -c '<the same loop>'
cost: mem.rss (poll@1ms) via `fs:proc:smaps_rollup` spent 9.4ms of cpu
      against the target's 100.3ms (9.40 %, declared `free`)
```

Two orders of magnitude, one band. The self-benchmark says the same thing about
`mem.pss`, where both backends exceed what they declare at a twenty-millisecond
poll while keeping exactly the ordering the catalog claims:

```
self-benchmark: `mem.pss` through 2 backends, one target, one window
  BACKEND                  DECLARED   MEASURED   EVENTS           MEAN
  fs:proc:smaps_rollup         free    0.633 %       56       4.6987e5
  fs:proc:smaps                 low    2.202 %       56       4.4869e5
  the backends agree to within 4.51 % on the value they report
```

So the *ordering* the catalog declares is right -- walking every mapping costs
about four times reading the kernel's rollup of them -- and the *absolute band*
is a statement about a poll rate nobody wrote down.

Two ways out, and this is a decision before it is code. Either a backend
declares its cost per unit of work -- per read, per event -- and the tool
multiplies by the rate the request asks for, which makes the declaration
checkable and the prediction possible before the run; or the band keeps its
present meaning and says out loud which rate it is a band *at*, which is cheaper
and leaves `--budget` unable to predict anything in advance. **Belongs to
whoever owns `catalog.rs` and `CATALOG.md`.** Nothing about the enforcement
landed here depends on the answer: it compares against a measurement, not
against the band.

### The per-event numbers this project already has, for whoever takes that decision

The first option needs a price per unit of work, and two of them are measured
and sitting in finding 6 rather than in the catalog:

| Mechanism | Per event | Source |
|---|---|---|
| preload shim | **132 ns** | finding 6, same event as the line below |
| eBPF uprobe | **740 ns** | finding 6 |

That ratio is the whole argument for the decision. `bpf:usdt` is implemented as
a uprobe on a `nop`, so it pays the same trap, and the entries that declare it
`low` are not wrong -- `low` means "under 1 %", and 740 ns at a thousand events
a second is 0.07 % -- they are *unfalsifiable*, because the same backend at a
hundred thousand events a second is 7.4 % and still declares `low`. A band
without a rate cannot be checked against anything, which is this finding's
point, and `usdt` is where it bites hardest: `runtime.alloc` and `proc.syscall`
name mechanisms whose event rate varies by three orders of magnitude between two
reasonable workloads.

It is also why `mem.heap.*` is served by a shim and not by a uprobe, which was
decided on this measurement before it was written down as a cost.


## 29. `Cost::Free` had no number, so its claim could not be checked

`free` is documented as "a counter read, about zero". As a number that has to be
*something*, and at exactly zero the claim can never be met by any real
measurement -- every `free` backend would read as having broken its own promise
the moment the promise became checkable.

`main.rs::worst_case_percent` now reads it as "under 0.1 %", which makes "about
zero" an assertion rather than a figure of speech. That is a number living in
the CLI about a catalog concept, which is the wrong place for it: the bands
belong beside the variants in `catalog.rs`, so the validator, the resolver and
anything else that ever wants to check a declaration reads the same numbers.
The other four already have documented bounds in the same doc comment and are
duplicated in `main.rs` for the same reason.

## 30. Exactly one implemented chain has a cheaper backend *below* the selected one

Budget degradation moves a collection down its chain. Of the 44 sources with a
collector, four have two:

| Source | Chain, in order | Where the cheaper one is |
|---|---|---|
| `mem.rss` | `fs:proc:smaps_rollup` (free), `fs:proc:statm` (free) | below -- the one case |
| `mem.pss` | `fs:proc:smaps_rollup` (free), `fs:proc:smaps` (low) | above |
| `mem.uss` | `fs:proc:smaps_rollup` (free), `fs:proc:smaps` (low) | above |
| `power.energy` | `pmu` (free), `fs:sys` (free) | neither |

So the degradation path has one end-to-end customer in this build, and
`tests/budget.rs` uses it. That is not an argument against the machinery --
`STATE.md`'s phase 6 item is *"degrading `trace` to an in-kernel `hist` where
the budget is exceeded"*, which is a mode degradation rather than a backend one
and has no producer until stream A's `bpf:tp` lands. It is an argument for
knowing that the first eBPF chain with a userspace fallback underneath it is
what makes this feature general, and that nobody should be surprised by how
little exercises it today.

## 31. Instrumentation inside the target is charged to the target

`getrusage(RUSAGE_SELF)` measures what *this process* spends, which is the right
answer for every collector that reads a file, drains a ring or polls a counter.
It is the wrong answer for `mem.heap.alloc`, `.live` and `.retained`: the
`LD_PRELOAD` shim runs inside the observed program, so what it costs is charged
to the program.

That is worse than it being missing, because the same cpu is the denominator.
An expensive shim inflates the workload it is measured against and therefore
reads as *cheaper* the more it costs. The three heap sources currently report an
overhead near zero for exactly this reason, and a `--budget` on them is
measuring the wrong process.

Every in-target mechanism has this shape -- the preload shim now, uprobes and
USDT when stream A lands them. The fix is for the shim to accumulate its own cpu
time and publish it through the ring it already owns, and for `Overhead` to grow
an `in_target_cpu_ns` beside `collector_cpu_ns` so the two are never added up by
accident. **Belongs to whoever owns `shim/` and `src/collect/mem/heap.rs`**; the
`Overhead` half is a field and a line in `Spending::spent`.

## 32. A target that burns no cpu has no denominator, and a budget over it cannot be checked

`--iface lo` consumes no cpu of its own; a mostly-idle `--pid` consumes less than
one clock tick over a short window. Both are reported rather than divided by:

```
cost: net.bytes (poll@50ms) via `fs:proc` spent 477.0us of cpu against nothing
      measurable: a network interface consumes no cpu time of its own, so there
      is nothing for an overhead to be a percentage of
```

The budget then holds vacuously -- it is never exceeded, because the ratio never
exists -- and says so rather than reporting that the ceiling was met. The
alternative was to fall back to wall time as the denominator, which would answer
a different question under the same name: a collector costing one millisecond a
second is 0.1 % of an idle machine and 0.1 % of a saturated one, and only one of
those is a measurement anybody wants.

Left as it is deliberately. If a wall-time ratio is wanted it should be a second
number with its own name, not a fallback for this one.

---

## Needs from another stream

### For whoever updates `docs/families/cpu.md`

Stream C did not edit it: the stream owns `src/perf.rs`, `src/collect/cpu/**`,
its own catalog entries, this file and its log, and the family note is not on
that list. Four passages in it are now false, and each has a replacement here so
the edit is mechanical.

1. **"What runs today" lists ten of fourteen sources.** It is thirteen. Add
   `cpu.profile` (`pmu:sw`, sample, landed in wave two), `cpu.oncpu_time`
   (`fs:proc`, poll, this wave) and `cpu.topdown` (`pmu`, count, this wave --
   written and collectable, out of reach on this machine).

2. **"Needs from the shared layer", item 1, "A `cpu` correlation key".** Closed
   twice over: `KeyValue::Cpu` exists (finding 2 above), and `cpu.util --cpu`
   now produces one event per core keyed on it. Delete the item.

3. **"Needs from the shared layer", item 2, "Per-cpu counting".** Built.
   `perf::PerCpuGroups` opens one group per cpu, `collect/cpu/percpu.rs` reports
   one event each, and `--system` is every online cpu. Replace with what it
   costs: the privilege refusal names `perf_event_paranoid` or `CAP_PERFMON`,
   and a per-task count is never substituted for one.

4. **"Not implemented yet, and why", the `cpu.topdown` paragraph.** It says the
   requirements are "raw event encodings per microarchitecture" and that
   `capabilities.rs` answers unverified. Neither holds: the five events are read
   from what the kernel publishes under `/sys/bus/event_source/devices/cpu/
   events`, which is how "map per CPU family, never hard-code event names" is
   honoured, and the probe answers `missing` by name. What is true and worth
   keeping: the method needs a pipeline that counts issue slots -- Intel from
   Icelake, AMD from Zen 4 -- and the development machine is a Zen 2, so the
   half of the ground-truth test that runs here is the refusal.

   The same paragraph's `cpu.oncpu_time` line ("`bpf:tp` first, and the eBPF
   layer is Wave 2") should say that the `fs:proc` backend now serves it in
   `poll`, at the kernel's tick granularity, and that `trace` is still stream
   A's to build.

### For stream A, or whoever owns the `io` entries: two globbed requirements

Finding 16 above, second half. `tracepoint:ext4:*` and `tracepoint:filemap:*`
are probed as literal paths, so `io.fs` and `io.readahead` report a requirement
missing that this machine has seven of. Naming the events the collector attaches
to is the fix; stream C did the same to `cpu.topdown` and did not touch `io`.

### For stream B, in `src/collect/mem/**`

`mem.cache_l1`, `mem.cache_llc` and `mem.cache_tlb` are collected, and **not
from your directory**. They are rows in `src/collect/cpu/counters.rs`'s `SPECS`,
as `BRIEF.md` says they should be: the mechanism decides who writes a source. If
`mem`'s `factories()` grows a line for any of the three there will be two
factories for one `(source, backend)` pair. Their catalog entries in
`catalog/mem.yaml` were edited by stream C -- requirements, emits and caveats --
and `mem.cache_llc` gained a second backend, `pmu:generic`.

### For the coordinator, on finding 10

Confirmed again this wave, from a third worktree. `sudo -n setcap …
target/debug/omn` still fails because `$(BINARY)` is relative and the sudoers
rule is absolute; passing `$(abspath $(BINARY))` by hand succeeds immediately.
Every stream in this wave that needs `CAP_PERFMON` is granting it by hand or
running without it, and a suite that is green without it is not evidence about
the privileged paths. One word in the Makefile.

---

Written by the network breadth stream, which owns `src/collect/net/**`. Each of
these is the other half of something landed there.

### To whoever owns `src/capabilities.rs`: a probe hook for the `net` family

`capabilities.rs` says, in a comment that is still correct:

> `ethtool` is deliberately absent: it is a generic-netlink family, resolved by
> name through `CTRL_CMD_GETFAMILY`, not a protocol number. Answering it as
> `NETLINK_ROUTE` would report a different question's answer, and reporting it
> unverified is the honest state until the family lookup exists.

The family lookup now exists, in `src/collect/net/ethtool.rs`, because
`net.hwts` had to do it to ask anything at all. What is missing is the line that
lets `capabilities.rs` use it — the same hook `gpu` already has, two lines up
from where this would go:

```rust
// src/collect/mod.rs
pub mod net;                       // today: `mod net;`

// src/capabilities.rs, beside the gpu hook
if let Some(probe) = crate::collect::net::probe(requirement) {
    return probe;
}
```

and, in `src/collect/net/mod.rs`, which this stream owns and will add as soon as
the caller exists (it is dead code until then, and the build refuses dead code):

```rust
/// Requirement classes this family answers for itself.
pub fn probe(requirement: &str) -> Option<crate::capabilities::Probe> {
    if requirement != "ethtool" {
        return None;
    }
    Some(match ethtool::Ethtool::open() {
        Ok(_) => crate::capabilities::Probe::Available,
        Err(error) => crate::capabilities::Probe::Missing(format!(
            "the `ethtool` generic-netlink family could not be resolved: {error}"
        )),
    })
}
```

It costs one socket and one round trip, changes nothing on the machine, and
turns the last unverified requirement in the `net` family into a checked one.
`omn list net` reports `net.hwts` as `?` today for exactly this reason, and
`net.iface`'s `netlink` backend is held back by it as well.

### To the coordinator, who owns the `Makefile`: `CAP_NET_RAW`

`net.capture` needs `CAP_NET_RAW` and the build grants `cap_bpf,cap_perfmon+ep`.
On this machine the source therefore reports the privilege it is missing rather
than collecting, which is correct behaviour and a source nobody can run.

This stream did not widen the capability set, for a reason worth stating rather
than just obeying: `CAP_BPF` and `CAP_PERFMON` let a binary observe the machine,
and `CAP_NET_RAW` lets it *send* arbitrary frames as well as read them. They are
not the same kind of grant, and the difference belongs to whoever decides what
this tool is allowed to be, not to the stream that wanted a test to pass.

Three options, in the order this stream would rank them:

1. **Leave it.** `net.capture` stays a source that names its missing privilege.
   The ground-truth test still runs, because it creates a user namespace where
   it holds the capability (below), so the code is exercised on every run of the
   suite on any machine with unprivileged user namespaces enabled.
2. **Grant it, and say so.** One more capability in `CAPS`, and a line in
   `CLAUDE.md` about what it allows. The sudoers line in the `Makefile` would
   have to change with it, and that line is already noted as hard to get right
   (finding 10).
3. **A separate target.** `make caps-capture`, run deliberately, so a developer
   who wants the capture opts into it and nobody gets it by building.

Whichever is chosen, the ground-truth test needs no change: it uses the
namespace only when the process cannot open a packet socket directly.

### To whoever owns `src/resolve.rs`: `placement()` will be wrong about `af_packet`

```rust
fn placement(backend: &Backend, entry: &Entry) -> FilterPlacement {
    if backend.id.starts_with("bpf") { FilterPlacement::Kernel(..) } else { .. }
}
```

Nothing is wrong today: `net.capture` attaches a classic BPF program that caps
the copy at the headers, which is not a predicate from the request, so reporting
the request's filter as evaluated in userspace is accurate. It stops being
accurate the moment a `--filter dport == 443` is compiled into that same
program, which is the natural next step and the one the brief named. The string
prefix is what has to go: a backend that filters in the kernel should say so
itself rather than be recognised by the first three letters of its id. The
smallest change that would do is a `kernel_filter: true` on `catalog::Backend`,
defaulting false, read here.

Until that exists this stream will not push a predicate down, because a filter
the tool describes in the wrong place is worse than a filter that costs more
than it should.

### To whoever owns `src/emit.rs`: a missing value is not a non-numeric one

```rust
fn value_of(event: &Event, attr: &str) -> Result<f64, EmitError> {
    event.attr(attr).and_then(|value| value.as_f64())
        .ok_or_else(|| EmitError::NotNumeric { attr: attr.to_owned() })
}
```

One event without the value attribute fails the aggregation of the entire run,
with a message that says the attribute is not numeric — which is a different
complaint from the true one, that it is not there. This shaped `net.hwts`: the
first design reported every interface, with an offset on those that keep a
hardware clock, and `--emit summary` then answered

```
error: `offset_ns` is not a numeric attribute, so it cannot be aggregated
```

for a run in which most interfaces were reported correctly. The source was
reshaped to refuse a scope with no clock in it, which is the better design for
independent reasons, so nothing is blocked. The gap is still real, and it will
find the next source whose events are not all alike:

```rust
#[error("no event carried `{attr}`, so there is nothing to aggregate")]
MissingValue { attr: String },
```

with the emit skipping events that do not carry it and the count of skipped ones
reaching the output, the way `Filtered` already does for a predicate. An
aggregate over three of forty events looks exactly like an aggregate over three.

### To whoever owns `CATALOG.md`: the `net.hwts` row

The prose table says

| `net.hwts` | NIC hardware timestamps | `SO_TIMESTAMPING` + PHC | trace | pid (socket opt) | free | high |

and the entry now declares two backends: `so_timestamping` for exactly that, per
process, with no collector; and `netlink:ethtool` for what an interface can
stamp and where its clock is, per interface, with one. The row is not wrong
about the source, it is silent about the half that is built. This stream owns
the entry and not the document, so the divergence is recorded rather than
resolved.

### To whoever owns `src/render.rs`: a series whose interesting part is its variation

`net.hwts --emit timeline` is the first source in the tool whose value is a
large absolute number of which only the changes matter. The text renderer scales
a `ns` unit to whatever reads best, and at this magnitude that is minutes:

```console
$ omn net.hwts --system --mode poll@100ms --duration 400ms --emit timeline
  timeline
    iface:enp38s0
      +0ns        29829246.93min
      +100ms      29829246.93min
      ...
```

Five readings, all identical to the digit shown, of a quantity that moved by
tens of nanoseconds between them — which is the measurement. The JSON carries
the exact value and the test reads that, so nothing is wrong with the numbers;
the text rendering of this particular shape is useless. Not this stream's file
and not urgent, but the shape will recur: any absolute offset, any counter read
as a level, anything whose zero is arbitrary. A renderer that showed the first
value in full and the rest as deltas from it would fix the whole class.

---

## 33. What the profile and trace encoders found

Written by stream D, which owns `src/encode.rs` and `src/format.rs`. `pprof` and
`chrome` were the last two declared formats without an encoder; both have one
now, and writing them turned up four things about the layers underneath.

### The result model has no wall clock and no sampling period, and pprof wants both

`profile.proto` carries `time_nanos`, the wall-clock instant a profile started,
and `period`, the interval between samples. This tool has neither to give.
`CLOCK_MONOTONIC` is the only clock domain it admits, so there is no realtime
instant to write; and the sampling period lives in the `Request`, which the
encoder is not handed -- `Results` carries the label, the source, the backend,
the unit, the window and the losses, and nothing about how the window was
sampled.

Both fields are therefore left at zero, which pprof reads as unknown, and the
profile's comment list says why. That is the right answer for `time_nanos`: a
wall-clock start would be invented. It is a **losable** answer for `period`,
because a continuous-profiling pipeline uses it to compare two profiles taken at
different rates, and the number does exist one layer up. Whoever owns
`src/emit.rs` could close it:

```rust
pub struct Results {
    // ...
    /// The interval between samples, for a sampling mode. Absent for a poll or
    /// a trace, which have no period.
    pub period_ns: Option<u64>,
}
```

`emit::aggregate` already takes the `ResolvedRequest`, so the value is in scope;
nothing but the encoders would read it.

### `Output::Stack` carries a count that only one encoder can keep

A `StackCount` has both `events` -- how many events landed on that stack -- and
`total`, what their value attribute summed to. `folded` writes one number per
line and `speedscope` one weight per sample, so both drop `events`. For
`cpu.profile` the two are equal and nothing is lost; for a source whose unit is
not `count`, `sched.offcpu` above all, "this stack blocked for 300 ms" and "this
stack blocked four times" are different facts and only the first survives.

`pprof` keeps the second as a numeric sample label, which is what pprof labels
are for; `go tool pprof -tagshow events` reads it back. Nothing needs to change,
but the asymmetry is worth knowing before anyone concludes the three stack
formats are interchangeable.

### Four of the seven units have no pprof name, and two of them have no profile

pprof scales and labels an axis from a fixed set of unit names. `ns`, `bytes`
and `count` map onto `nanoseconds`, `bytes` and `count`. `ratio`, `hz`, `joules`
and `celsius` do not, and the encoder writes them under their own names with a
comment saying no tool will scale them, rather than borrowing the nearest name
pprof would recognize -- energy reported as `count` renders as a count of
nothing.

Two of the four are worse than unlabelled, and it is a modelling question rather
than an encoding one: **a ratio and a temperature do not sum**, and a profile is
a sum over stacks. No source declaring `stack` has either unit today, so nothing
is wrong now; a source that did would produce a profile whose every number is a
sum of things that cannot be added. The `emits`/`unit` pair in a catalog entry is
where that could be refused.

This is also the second reason for `Unit::Watts`, already asked for further up
this file: a `power.*` source with a `stack` emit would be exactly the case
above, and watts is the unit that makes `rate` over joules mean something.

### The encode layer returned text, and one declared format is not text

`encode::run_output` returned a `String`, and a gzipped protobuf cannot be one.
`encode::run_output_bytes` is now the general entry point -- it returns the
bytes of any of the nine formats, and the text entry point stays for the eight
that are text and refuses the ninth by name. `OutputFormat::is_binary` and
`OutputFormat::extension` are the two questions a caller needs to answer to
write a file.

**This required four lines in `src/main.rs`, which stream E owns.** They are
listed under "Needs from another stream" below; they are already applied,
because without them a format the tool lists as implemented could not come out
of the command line, and the test that crosses a format's claim with reality
exists for exactly that kind of gap. If stream E's own work conflicts there, its
version wins -- the only thing that must survive is that the run's bytes reach
stdout as bytes.

### Measured

- **The Chrome trace's axis holds across requests.** A trace gives every request
  its own process track, and each track's zero is that request's own first
  event, which is only one axis if the requests start together. Two `poll@50ms`
  requests over one 400 ms window, on this machine: the last point of one track
  is at 400 463.6 µs and of the other at 400 462.5 µs, so the two axes agree to
  **1.1 µs over 400 ms**. `otherData.time_origin` states the convention rather
  than leaving a reader to assume it.
- **The gzip container is written with stored deflate blocks**, which is a
  conformant gzip stream produced without a compressor -- `gzip -t` accepts it
  and `go tool pprof` reads it. It costs five bytes per 64 KiB over the raw
  protobuf and buys no dependency. A 244-stack profile of python came to 19 186
  bytes.
- **The frame-pointer degradation reaches the profile.** `cpu.profile` on a
  python built without frame pointers reports "139 of 270 samples landed in a
  function that sets up no frame pointer", and that sentence is in the pprof
  file's comment list, where anyone who opens the profile in `pprof` or Parca
  reads it before believing the shape.

## Needs from another stream

Written by stream D, which owns `src/encode.rs` and `src/format.rs`.

### To stream E, for `src/main.rs` -- already applied, see above

```rust
use std::io::{IsTerminal, Write};

let output = encode::run_output_bytes(session.format, &run)?;
if session.format.is_binary() && std::io::stdout().is_terminal() {
    anyhow::bail!( /* redirect it to a file or pass --out <dir> */ );
}
std::io::stdout().write_all(&output)?;

fn write_artifacts(/* ... */ encoded: &[u8]) -> anyhow::Result<()> {
    // results.{format.extension()} rather than results.{format}
}
```

`format.extension()` exists for the second half: a Chrome trace is `.json` and a
speedscope file is `.speedscope.json`, because that is what the tools that read
them open by, and `results.chrome` opens in nothing.

### To whoever owns `README.md` and `DECISIONS.md`

Both say result formats "report that their encoder is not written". Every
declared format has one now. `README.md:163` and the "Output formats" row of
`DECISIONS.md` need the sentence about *why* the mechanism exists -- a format is
declared before it is written, and the listing says which -- without the claim
that any format is currently in that state.

### To whoever owns `src/render.rs`

`render::formats()` ends with "`chrome` carries timelines". It carries `raw` as
well, which is the emit that puts individual events on the trace with what they
measured and what they can be joined on. One word:

```
`chrome` carries timelines and raw events
```

---

Written by stream E, which owns `src/collect/mod.rs` and `src/main.rs`. Two
edits outside that were made anyway and are declared at the end.

### To whoever owns `src/request.rs`

`Budget`'s doc comment describes a tool that does not exist:

```rust
/// The resolver degrades to stay under it (`trace` to in-kernel `hist` to
/// `sample`) and reports what it degraded. It never silently drops events to
/// fit.
```

It is the *runner* that degrades, not the resolver, and it moves along the
backend chain rather than down the modes -- the mode degradation needs an
in-kernel `hist` producer, which is stream A's. What is true now:

```rust
/// An overhead ceiling, as a percentage of the observed workload's own cpu
/// time. The run weighs itself against it while it runs; over the ceiling it
/// moves to a cheaper backend in the chain, and where there is none it stops.
/// Either way it says which. It never silently drops events to fit.
```

### To whoever owns `src/catalog.rs`

```rust
impl Cost {
    /// The top of the overhead band this stands for, as a percentage. `Extreme`
    /// is "over 10x" and is not bounded.
    pub fn worst_case_percent(self) -> f64;
    /// The band in words, for a message: "1 to 5 %".
    pub fn band(self) -> &'static str;
}
```

Both exist in `main.rs` today, where the CLI is asserting numbers about a
catalog concept. Finding 16 is why `Free` needs a number at all.

### To stream D, who owns `src/emit.rs` and `src/encode.rs`

`Results` has nowhere to carry what a measurement cost, so the cost reaches the
user on standard error and in `<out>/cost.json`, and never in `--format json`
results. A pipeline reading the results alone cannot tell a 0.1 % measurement
from a 9 % one. What is wanted is one field, passed straight through from
`Collection`:

```rust
pub struct Results {
    ...
    /// What this measurement cost, measured rather than declared.
    #[serde(default)]
    pub overhead: collect::Overhead,
    /// The backends that produced it, in order; longer than one when a budget
    /// moved the collection mid-run.
    #[serde(default)]
    pub served_by: Vec<String>,
}
```

`aggregate` already takes the `Collection` by value and both fields are on it.
`encode.rs`'s text renderer is where the `cost:` line would belong once it has
somewhere to read it from, and `main.rs` would stop printing it separately.

### To the coordinator

- `ROADMAP.md` phase 6 item 3 and phase 8 are done and unticked. Phase 8's
  "self-benchmark" is a session naming one source several times with
  `--backend force:`, not a subcommand.
- `CATALOG.md`'s flag table says `--budget 2%  overhead budget; the resolver
  degrades and says so`. The resolver does not; the runner does.
- `STATE.md` item 5, "`--budget` is parsed, explained, and not enforced", is
  closed. Findings 15 and 17 are what replaces it.

### Declared: two edits outside stream E's paths

Both are one-line truths about a flag whose behaviour this stream changed, and
leaving either wrong is the exact failure this repository keeps a test against.

- `src/cli.rs`: the `--budget` doc comment said "Not yet enforced". The
  integration test `the_help_and_the_run_agree_that_a_budget_is_enforced` exists
  to make the help and the run change together, and it was written that way on
  purpose by whoever wrote its predecessor.
- `man/omn.1`: `--budget` said "Parsed and recorded; not yet enforced";
  `--backend` now names the self-benchmark, and `FILES` documents
  `<out>/cost.json`.

No other stream in this wave owns either file.

---

## Needs from another stream

Written by stream A, which owns `src/main.rs`, `src/cli.rs`,
`src/collect/mod.rs` and `src/error.rs`. `docs/fixes/runner.md` has the
reasoning; these are the exact edits.

### To stream D, for `src/collect/sched/switches.rs`, and to stream E, for `src/collect/mem/page_faults.rs`

Both refuse a mode this build has not written, from `Collector::start` rather
than from the factory — and a refusal raised against a target that already
exists is a failure of this run until the collector says otherwise, because a
counter that would not open reaches the same line. So they still exit 1, which
is sched 9 and three of mem D5's sixteen. One line each:

```rust
// src/collect/sched/switches.rs, in `start`
other => return Err(collect::not_built(format!(
    "`count` mode for `pmu:sw` needs the perf layer's counting support \
     (see STATE.md); ask for `--mode trace` to get one event per switch"
))),

// src/collect/mem/page_faults.rs, in `start`
other => return Err(crate::collect::not_built(format!(
    "`{other}` mode does not sample page faults"
))),
```

`collect::not_built` returns a `String`, so nothing about the signature moves;
the sentence reaches the reader unchanged and the exit code becomes 3. Its
sibling `collect::cannot_mean_anything` is for the other direction — a request
no build and no machine can make mean something — and both are documented in
`src/collect/mod.rs` beside `Whose`.

The scenarios that pin these are `tests/scenario_sched.rs`'s
`a_source_with_no_collector_is_refused_with_its_own_exit_code` (sched 9 belongs
beside it) and `tests/scenario_mem.rs`'s
`every_mode_and_emit_of_the_counter_sources_is_served_or_refused_by_the_right_code`,
whose pinned list is down to the three `mem.faults` combinations and should
become empty.

### To stream B, for `catalog/io.yaml`

Two `io` factory refusals are requests that cannot mean anything rather than
gaps in this build: `io.inflight -m count` (a queue depth is read at an instant,
so it counts over no window) and `io.bytes -m snapshot` (one read of
`/proc/diskstats` is a counter rising since boot, not what the source did during
a run). They exit 3 now, where they exited 1 before; neither is right. The
honest fix is the entry, which declares a mode no backend of it can serve —
this is finding 1 of this document seen on the mode axis. If the entries stay
as they are, `collect::cannot_mean_anything` at the two refusal sites in
`src/collect/io/diskstats.rs` gives them exit 2.

### To whoever owns `src/resolve.rs`: a backend that cannot serve the requested mode

mem D7. `mem.numa -m sample@997hz` resolves onto `fs:proc`, which reads a file
and has no sampling rate, because modes are declared per entry and not per
backend. The runner now says so in a degradation that travels with the results,
which is the half this stream could reach; the resolver should not have selected
it. Every source with a sampling backend it has not implemented and a polling
fallback it has will do the same thing, so this is not `mem`'s.

### To the coordinator, who owns `man/omn.1` and `GUIDE.md`

The exit codes changed, and both documents carry the old table. A documented
contract that has quietly stopped being true is the same kind of lie as a wrong
number, so this is worth doing before the merge is announced rather than after.

`man/omn.1`, `EXIT STATUS`:

```troff
.TP
.B 1
The request is valid and this machine cannot serve it, or a run failed.
.TP
.B 2
The request was refused: it cannot mean what it asks for.
.TP
.B 3
The request is valid and nothing in this build collects it yet.
```

`GUIDE.md`, "The refusal — *is the reason the right reason?*": the four-row
table now reads 2, **1**, 3, **3**, the fourth row is "a collector was chosen
and refused a shape this build has not written", and the two paragraphs below it
that explain which two codes are wrong come out. `docs/fixes/runner.md` has the
table as it now stands.

Also: `STATE.md` §"What remains", item 1, opens with the two recurring defects —
"A refusal's exit code does not match its message" and "A poll that cannot keep
its cadence" — and both are closed.

---

Written by stream B, which owns `catalog/*.yaml` and `src/validate.rs`.

Five of the eleven defects this stream was given cannot be closed inside those
two files. Each is recorded here with the signature that would close it, and in
`docs/fixes/catalog.md` with what the catalog did instead in the meantime.

### To whoever owns `src/request.rs`: `count` mode must be able to rank

```rust
ModeKind::Count => &[EmitKind::Rate, EmitKind::Summary, EmitKind::Raw, EmitKind::TopK],
```

One arm of `ModeKind::valid_emits`, and it closes net 1, sched 5 and the
`--emit topk` half of io 1.

The reasoning is the one already written into that match for `raw`: a counted
window is one event *per series*, not one number. `emit::topk` groups by series
key and sums the value attribute; `emit::rate` groups by series key, sums the
value attribute and divides by the window. They are the same function with one
extra division, and only one of them is reachable from `count` -- so a counted
run can be told what its heaviest series *rate* was and cannot be told which
series was heaviest. `sched.migrations --mode count --emit summary` renders
twenty-odd per-task series today and refuses to rank them; `net.bytes` produces
one series per interface and refuses the same.

Until that lands, two entries stopped promising a ranking no mode of theirs can
reach: `net.bytes` no longer declares `topk`, and `docs/scenarios/net.md` keeps
the reproduction. The day the arm changes, `topk` goes back into
`catalog/net.yaml` and the `RECORDED` constant of
`every_emit_a_net_source_declares_is_reachable_from_a_mode_it_declares` stays
empty either way.

### To whoever owns `src/catalog.rs`: two lines, one of them a field

The first makes the check this stream wrote a refusal rather than a test:

```rust
// in Catalog::from_entries, after check_backend_filter_fields(entry)?
let complaints = crate::validate::check_catalog_entry(entry);   // or over the whole catalog
```

`src/validate.rs::check_catalog` is written and covers two promises an entry can
make that no build can keep -- an emit no declared mode can produce, and a
`rate` over a unit that does not add up. It runs today from
`validate::tests::the_catalog_promises_nothing_it_cannot_produce`, which guards
the embedded catalog and nothing else; a catalog loaded with `--catalog <dir>`
is not checked at all. The loader is where this belongs, beside
`check_backend_targets` and `check_backend_filter_fields`, which are the same
kind of check written the same way.

The second is the field that io 8 needs:

```rust
pub struct Backend {
    // ...
    /// Which of the entry's keys *this* backend attaches to an event.
    ///
    /// Absent means all of them, as for `filter_fields`. Present, it is a
    /// subset: a key the selected backend never attaches makes every join
    /// over it attribute nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keys: Option<Vec<KeyKind>>,
}

impl Entry {
    pub fn backend_keys<'a>(&'a self, backend: &'a Backend) -> &'a [KeyKind];
    pub fn backends_carrying_key(&self, key: KeyKind) -> Vec<&str>;
}

fn check_backend_keys(entry: &Entry) -> Result<(), ParseError>;   // both directions
```

Exactly the shape `filter_fields` already has, including the loader check in
both directions. `io.bytes` is the entry that wants it: the chain carries `blk`,
`task`, `cgroup` and `dev`, `fs:proc` attaches `dev` alone and `fs:cgroup` `dev`
and `cgroup`, and only the tracepoint backend -- which this build does not
implement -- carries `task`.

### To whoever owns `src/resolve.rs`: refuse a key the selected backend cannot attach

With the field above, the other half of io 8, beside `check_filter_fields`,
which is the same function over the other list:

```rust
fn check_keys_carried(entry: &Entry, backend: &Backend, request: &Request) -> Result<(), ResolveError>;

// and, in src/error.rs, the twin of FilterFieldNotCarried:
ResolveError::KeyNotCarried { source_id, backend, key, elsewhere: Option<String> },
```

The validator cannot do this: it runs before a backend is chosen, and which keys
are real depends on which one the resolver kept. `--keys task` on `io.bytes`
resolved onto `/proc/diskstats` is accepted today and produces events with no
task on them, which is the expensive silence -- a session written now to be
joined later produces events that join to nothing, and re-running it is the same
work twice. `--filter "comm == ..."` on the same backend is refused and names
the backend that would carry the field; this is that message, for keys.

### To stream F, for `src/collect/net/`: what a snapshot of a counter answers

net 3. `net.iface --mode snapshot` and `net.backlog --mode snapshot` report the
increment between their one read and the closing read that follows it about a
hundred microseconds later, which on anything short of a saturated machine is
zero. A reader taking a snapshot to ask *is this machine refusing connections
right now* is told no.

The answer this catalog already gives elsewhere is the totals since boot:
`examples/mem-machine-counters.yml` has a request labelled "swap since boot"
which is `mem.swap --mode snapshot --emit raw`, and `io.pressure` states the
rule in its own caveat -- *"A poll reports the share of its own window... A
snapshot has no window, so it reports the kernel's decayed ten-second average
instead, under its own name: the two are different measurements and are never
given the same one."*

So: on `snapshot`, report the absolute counter under its own attribute name
rather than a delta over a window the mode does not have. The catalog will carry
the caveat that says which of the two a reader is holding, the way `io.pressure`
does, once there is a second measurement to name.

The alternative -- dropping `snapshot` from both entries -- was not taken
because it breaks `examples/net-snapshot.yml`, which this stream does not own,
and because a snapshot of a counter is a question worth answering rather than
one worth refusing.

### To stream F, for `src/collect/net/iface.rs`: one event per counter kind

net 5. The event's value is the error count and drops, pause frames and the
per-direction figures are attributes beside it, so `rate`, `timeline` and
`summary` -- everything but `raw` -- answer about errors alone. A link that had
just dropped exactly a thousand frames reads `0 count/s`.

Keeping them apart is right and the catalog says so. What costs the aggregates
is that the separation is made with attributes, which no aggregating emit
carries. One event per counter kind would keep the separation and make all of
them visible; the series need something to be told apart by, and `iface` is
already the key, so this wants either a second key or the counter kind in the
key's own name. `catalog/net.yaml` carries a caveat saying what the number
answers about until then.

### To stream C, for `src/emit.rs`: a ranking over an attribute

runtime 1. `topk` ranks series, a series is a distinct set of keys, and
`runtime.jit` carries `task` alone -- so a run answers `topk:10` with one row
per process however many methods were compiled, and that row's total is the
event count `summary` already gives. The question a JIT map exists to answer is
*which methods dominated*.

`method` is an attribute rather than a key, and making it a key is the larger
change: a key is what joins across sources, and there is no `KeyKind` for a
compiled method. The cheaper half is `topk` being able to group by an attribute
-- `--emit topk:10@method` or equivalent -- which `io.bytes`, `io.iops` and
`net.iface` all want too, for `rwbs`, for the operation kind and for the counter
kind. Finding 8's "What would let `topk` answer the question `io` and `net`
declare" is the same request from the other side.

`catalog/runtime.yaml` carries a caveat saying what `topk` ranks here and what
it does not, which is the honest half this stream can do.

### To stream E, for `src/collect/mem/`: a counted window for `mem.swap`

mem D3, the half that is a judgement rather than a deletion. `mem.swap` measures
a flow -- pages written to, or read back from, a swap device during the window --
and a rate is the natural thing to want from it. It declared `rate` and declared
neither of the modes that can produce one, so the emit is out of the entry for
now.

The other fix is a `count` mode: two reads and one event carrying the window,
which is what `net.bytes` and `net.packets` already do over `/proc/net/dev`, and
what `mem.swap --mode snapshot` already does over `/proc/vmstat` for the totals.
If that lands, `modes: [poll, snapshot, count]` and `rate` goes back into
`catalog/mem.yaml`.

---

Written by stream C of the fifth wave, which owns `src/emit.rs`,
`src/filter.rs`, `src/encode.rs` and `src/render.rs`. Each of these is the other
half of a defect this stream was given and could not reach from those four
files; `docs/fixes/emit.md` has the reasoning, and what follows is only the
shape wanted, so that landing it is a small edit rather than a rediscovery.

### To stream D, in `src/collect/sched/offcpu.rs`: one attribute closes sched 1

`emit::TOTAL_OF_ATTR` now exists. A collector that carries it is saying *this
event's value is the sum over N measurements whose individual values do not
reach the emit layer*, and `emit::aggregate` then puts a degradation beside
`hist` and `summary` naming both counts. It is inert until a collector opts in,
and `sched.offcpu` is the source it was built for.

In `attributes()`, beside the `blocks` attribute that is already there:

```rust
// This row's `total_ns` is the sum of `blocks` blocks, and the durations
// that went into it were added up in the kernel. `hist` and `summary` say
// so rather than presenting the drain window as a measurement.
Attr::uint(crate::emit::TOTAL_OF_ATTR, entry.blocks),
```

That makes defect 1 of `docs/scenarios/sched.md` visible instead of silent, and
a scenario can then assert the degradation.

It was exercised here before being written down: the line above was added to
`offcpu.rs`, the workload of defect 1 was run, and the line was reverted, so
nothing in this stream's diff touches that file. What it produces, on a drain of
two blocks:

```
degraded: `hist` and `summary` describe the 1 row this backend handed up and not the 2
measurements behind them: the values are added together before they reach the emit layer,
so every bucket, mean and extreme here is of a total over a drain window rather than of a
measurement. `total_of` on each row says how many it stands for

  hist (log2)
    task:595596  n=1  min 109.96ms  mean 109.96ms  max 109.96ms
      [67108864ns, 134217728ns)             1  |################################|
```

The histogram is still wrong, which is the point: it now says so, and the run
carries the number that says by how much.
 It is not the fix. The fix is the
one `docs/open-findings.md` §11 already named — *"the fix is in the key, not in
the emit"*: a log2 bucket in the aggregation key, so the kernel counts
`(task, state, bucket) -> blocks` and the histogram falls out of the map, as
`sched.runqlat` does. When that lands, the row carries `WEIGHT_ATTR` instead and
`TOTAL_OF_ATTR` comes off; the emit layer needs no further change either way.

**Do not reach for `WEIGHT_ATTR` here**, which is what `BRIEF.md` suggests.
`WEIGHT_ATTR` means *this value occurred N times*, which is true of a `runqlat`
row and false of this one: weighting a drain of four hundred blocks by four
hundred reports four hundred measurements at the value of their *sum*, in a
bucket none of them was in. The arithmetic is pinned by
`emit::tests::a_row_that_is_a_total_over_its_measurements_says_what_it_is`.

### To stream A, in `src/collect/mod.rs` and `src/error.rs`: mem D4

The refusal sentence pushed into a slot meant for one word. `src/collect/mod.rs`
fills `CollectError::UnsupportedTarget`'s `{target}` with
`format!("{} ({reason})", target.kind())`, so the message ends `"...an already
running process) target yet"`. The variant wants the reason beside the target
rather than inside it:

```rust
#[error("`{source_id}` cannot be collected for a {target} target yet: {reason}")]
UnsupportedTarget {
    source_id: String,
    target: String,
    reason: String,
},
```

and the site at `src/collect/mod.rs:702` passes `target.kind().to_string()` and
the sentence from `Prepared::requires_launch` separately. Nothing in this
stream's four files formats this message. Pinned by
`asking_the_shim_for_a_running_process_is_refused_by_the_build` in
`tests/scenario_mem.rs`; every family with a launch-only collector reaches it.

### To stream A, in `src/collect/mod.rs`: what would close net 6 generically

net 6 is `daddr == '127.0.0.1'` keeping nothing forever, because the attribute
holds `127.0.0.1:38402`. The narrow fix belongs to stream F and is written
below. The general one is here, and it is the next case of something
`apply_filter` already does.

`Filtered::unseen_fields` exists because *"a predicate over a field nothing
produces is false everywhere, which empties the series and looks exactly like a
workload that did nothing"*. A predicate over a field that **is** carried, whose
string values never equal the literal but do contain it, is the same silence for
a nearly identical reason. `apply_filter` is the only place that still has the
events the predicate rejected:

```rust
/// Predicates that kept nothing although the field was there, with what
/// the values actually looked like. An exact match against an attribute
/// that carries more than the value compared is the shape this catches.
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub near_misses: Vec<String>,
```

populated with a sentence per `Compare { operator: Eq, value: Str(text), .. }`
that matched no event while at least one event's value for that field contained
`text`, naming an example value. `src/encode.rs` will render it under the
existing `filter ... kept N of M` line, beside `unseen_fields`, once the field
is there — that side is one edit in this stream's file and is not blocked on
anything else.

### To stream F, in `src/collect/net/`: net 6 itself

`net.tcp.rtt`'s `daddr` attribute carries the address and the port. `dport` and
`sport` are already separate numeric attributes, so the port inside `daddr` buys
nothing and costs the obvious filter — an equality against an address can never
match, forever, and the run reports it as an ordinary empty result. Either
`daddr` becomes the address alone, or `catalog/net.yaml` grows a caveat saying
what it holds; the first is better and cheaper. Reproduction and both outputs
are in defect 6 of `docs/scenarios/net.md`.

### To whoever owns `src/request.rs`: runtime 1, and io 7 with it

No stream in this wave owns `src/request.rs`, which is why runtime 1 is
unfixed. It needs `topk` to be able to rank by an attribute rather than only by
the series key:

```rust
pub enum Emit {
    // ...
    /// `topk:10` ranks series; `topk:10@method` ranks the distinct values of
    /// an attribute. A series is a set of keys, so a source carrying one key
    /// answers `topk` with one row however many distinct things it measured.
    TopK { k: u32, by: Option<String> },
}
```

with the parse accepting `topk`, `topk:N` and `topk:N@<attr>`, the `Display`
round-tripping all three, and `EmitKind::TopK` unchanged. `src/emit.rs`'s
`topk()` then groups by `event.attr(by)` instead of `SeriesKey::of`, which is a
small change in a file this stream owns and will be made as soon as there is a
way to ask for it. `src/validate.rs` should refuse an attribute the entry does
not declare among its `filter_fields`, which is the only list of attribute names
the catalog carries.

**This is the same gap as io 7.** `io.bytes` puts `read_bytes`, `write_bytes`,
`sector_bytes` and `discarded_bytes` on every event and declares
`[rate, summary, timeline, topk]`, none of which can show any of them; so *was
that reads or writes* is measured, attached, and unreachable. `io.iops` and
`io.merges` are the same shape. Whoever takes one takes both — and the earlier
request in §12, *"what would let `topk` answer the question `io` and `net`
declare"*, is the third face of it, from the key side.

### To whoever owns `man/omn.1`: the remaining half of runtime 2

`=~` is now refused when its pattern could only have been meant as a regular
expression, which is the only place a run has any reason to say what the
operator is. The manual still lists it among the operators without saying. In
the `--filter` entry, after the operator list:

```troff
.BR =~
is a substring test rather than a regular expression:
.RB ` "comm =~ 'python'" '
keeps
.BR python3 .
A pattern that can only be a regex \(em a character class, a group, an escape,
or an anchor \(em is refused rather than matched literally.
```

`tests/scenario_runtime.rs::defect_2_the_match_operator_is_a_substring_test_and_says_so`
asserts that the manual is still silent, and says in its own message to drop
that assertion when this lands. No stream in this wave owns the file.

### To stream A, for the exit code of a filter that cannot mean anything

Noticed while landing runtime 2 and left alone, because it is exactly stream A's
class. A `--filter` that fails to parse is a `ParseError` raised by clap's value
parser, so it exits **1** — "this machine cannot serve it" — where it is plainly
"the request cannot mean anything", which is 2. It is the same fall-through
`docs/scenarios/sched.md` defect 6 describes for a missing `--mode`, and it now
has a second and more visible instance.

---

Written by wave five's stream D, which owns `src/stack.rs`,
`src/collect/cpu/**`, `src/collect/sched/**` and `docs/fixes/stacks.md`.

### To whoever owns `tests/family_sched.rs` — the one-in-five flake

`tasks_sharing_one_cpu_wait_the_share_the_scheduler_owes_them` fails about one
run in five because `distinctly_named_shell` builds its workload at a
machine-wide path, `/tmp/omnrunqshare`, and identifies it with a machine-wide
`comm` filter. Six worktrees of this wave run that suite on this machine, and
each one's spinner answers to the same name; the test then finds two tids where
it pinned one. Demonstrated in `docs/fixes/stacks.md`, along with what it rules
out — the collector is innocent, `sched.runqlat` reads `comm` from
`task_struct.comm` in the kernel.

The name has to be unique per *run*, not per test. A `comm` keeps fifteen
characters, and the pid fits inside what the existing names leave:

```rust
fn distinctly_named_shell(name: &str) -> Option<std::path::PathBuf> {
    let name = format!("{name}{}", std::process::id() % 100_000);
    assert!(name.len() <= 15, "`{name}` is longer than a comm");
    // … unchanged, with `&name` used for the path and returned to the caller
}
```

Its three callers pass `"omnrunqshare"`, `"omnrunqbkt"` and `"omnmigrate"` and
each also writes that string into a `--filter`, so the generated name has to
reach the filter too — returning it beside the path, or having the helper
return a small struct, is the shape. The same applies to
`tests/scenario_cpu.rs::Spinner::started`, which builds `/tmp/omnprofilefmt` and
friends the same way; it does not filter on `comm`, so it is a latent version of
the same collision rather than an active one.

### To stream E, for `src/collect/mem/page_faults.rs`

`mem.faults` opens a per-task sampling event over a process, exactly as
`cpu.profile` and `sched.switches` do, and the kernel will not inherit it: the
faults of any thread or child the target starts afterwards are in no number the
run reports. The other two now say so (`cpu` 1 in `docs/scenarios/cpu.md`);
this one does not, and `STATE.md` claims it does.

The disclosure already exists and needs one caller. Read it at `start`, because
`stop` closes the event and the notes are gathered after it:

```rust
// in the collector, beside `event`
disclosure: Option<&'static str>,
more_than_one_task: bool,   // `!matches!(target, Target::Tid(_))`, from `attach`

// in `start`, after `event.enable()`
self.disclosure = Some(event.sampled_threads());

// in `notes`
if let (true, Some(limit)) = (self.more_than_one_task, self.disclosure) {
    notes.push(format!(
        "a sampling event is not inherited, so these faults are {limit}; …"
    ));
}
```

`src/collect/cpu/profile.rs` and `src/collect/sched/switches.rs` have the two
worked examples, including the count of tasks the run actually saw, which is the
measured half.

### To whoever owns `tests/scenario_cpu.rs` — a scenario that measures a rate without the machine

`a_profile_renders_in_every_format_that_carries_stacks` asserts that a 200 ms
window at 997 Hz holds between forty and three hundred and twenty samples, and
it holds `may_load_the_machine()` — the *shared* lock. `pressure_rises_when_
there_are_more_runnable_tasks_than_cpus` holds the same shared lock and starts
forty-eight spinners, so the two run together, and when they do the profile's
own loop can be starved past its deadline: three failures in about eighteen
full runs of that target during wave five, each with *too many* samples, the
worst being 1483 — a second and a half of the target's cpu inside a window that
asked for two hundred milliseconds.

The rest of this file's duration and share scenarios take
`machine_to_itself()` for exactly this reason, and the comment on that lock says
why. This one measures a rate over a window and should take it too:

```rust
fn a_profile_renders_in_every_format_that_carries_stacks() {
    let _alone = machine_to_itself();
```

Stream D of wave five did not make the change: the scenario does not name a
defect that stream fixed, and the seam in `BRIEF.md` is narrower than that.

---

## Needs from another stream — the fifth wave, stream E

Three things stream E (`mem`, `proc` and `power` readings) could not do from its
own paths. Each is written out here with the exact change wanted rather than
applied in passing. `docs/fixes/mem.md` has the reasoning behind all three.

### From stream A, in `src/collect/mod.rs`: a unit test that fails under load

`collect::tests::what_a_run_reports_spending_is_the_cpu_it_actually_burned`
drives a spinning collector until a **wall** deadline 200 ms out, accumulates
the **cpu** the collector actually burned, and refuses to conclude anything
unless that came to at least 150 ms. On an idle machine the two are one number.
On a busy one — eight streams running their suites at once, which is every wave
this project has had — the child is descheduled, burns less than the floor, and
the test fails with "the collector was asked to spin for 200 ms and did not, so
this measured nothing". Measured here at one failure in eight runs of
`cargo test --lib` alone. It is `power` 5 of `docs/scenarios/power.md`.

What the test is for does not depend on the wall clock at all: it checks that
the spending meter agrees with the collector's own account of the cpu it spent.
So the stop condition should be that account rather than the clock:

```rust
// in what_a_run_reports_spending_is_the_cpu_it_actually_burned, in the child
let (mut collectors, spun_ns) = burning(20_000_000);
let burned = spun_ns.clone();
drive(
    &mut collectors,
    Window { interval: StdDuration::ZERO, snapshot: false },
    &mut spending,
    move || (burned.get() >= 200_000_000).then(|| "the cpu was burned".to_owned()),
);
```

`NOTHING_WAS_BURNED` then becomes unreachable rather than load-dependent, and
the two ratios either side of it are unchanged. A busy machine makes the test
take longer in wall time, which is the right way for it to notice load.

### From stream A, in `src/collect/mod.rs`: a counted window rounds up to the tick

`power.energy -m count -d 600ms` observes 1.0002 s. The collector asks for
nothing between `start` and `stop` in `count` mode, so this is the runner: it
checks its deadline once per round, and with no collector declaring a pace it
takes the interval from `resolved[0].request.mode`, which for `Mode::Count`
falls through to `Mode::DEFAULT_POLL_INTERVAL`.

Stream E deliberately did not work around it by giving a counted collector a
short `tick()`. The run's interval is the minimum over its collectors, so one
counted `power.energy` request asking for 10 ms would drag every other request
in the session down with it — which is `power` 4 and `proc` 3, made worse. The
fix belongs with whoever gives the runner a per-request schedule: a request in
`count` mode needs no reads at all, and its window should end when its duration
does.

### For the coordinator, in `examples/`: two sessions describe a defect that is gone

`examples/` belongs to no stream in this wave, and both of these carry header
comments explaining at length a defect that is now fixed.

**`examples/proc-one-reading.yml`**, the paragraph beginning "Against a
*launched* one it is not". The single read no longer describes `omn`; it
describes the launched program at the instant it began, which for a `python3`
that has not run its first statement is its three standard streams. Suggested
replacement for that paragraph and the two lines after it:

```
# Against a *launched* one it is one read of the program's first instant. The
# child is held at a gate, and the read waits for it to become the program
# rather than the forked image of `omn` it starts as -- so `comm` names the
# program and the count is the program's. What it is not is a reading of a
# program that has done its startup: `python3` holds its three standard
# streams at `execve` and opens its files milliseconds later. When the question
# is "what is it holding now", ask it of a running process:
#
#   omn proc.fd --pid $(pgrep -n your-service) --mode snapshot --emit raw
```

**`examples/proc-exit.yml`**, the paragraph beginning "There is a third answer
procfs gives that this collector does not yet handle". The collector now reads
the process state and reports nothing for a target in `Z`, so the last point of
the series is the last reading the program was alive for. Suggested replacement:

```
# There is a third answer procfs gives, and it is the one this file was written
# for. A process that has exited but has not been reaped is a zombie, and a
# zombie's `/proc/<pid>/fd` is neither ESRCH nor ENOENT: it is an empty
# directory that reads successfully. Reported as a measurement it was a
# truthful-looking zero -- no open descriptors, full headroom, no loss -- for a
# program that never closed a handle in its life. The collector now reads the
# process state, so that round publishes nothing and the run ends for the
# reason the runner knows, which includes the exit status.
```

---

Written by stream F, which owns `src/collect/{net,io,gpu,runtime}/**`. Four
things a collector cannot reach, each behind a defect this stream was given.
`docs/fixes/net.md` has what was done on this side of each.

### To stream B, for `catalog/io.yaml` — the read/write split has no emit (io 7)

`io.bytes` attaches `read_bytes`, `write_bytes`, `sector_bytes` and, where the
kernel reports them, `discarded_bytes` to every event, and declares
`emits: [rate, summary, timeline, topk]`. None of those carries an event's
attributes; `raw`, which does, is not declared and is refused by name. So the
first question anybody asks of a volume — *was that reads or writes?* — is
measured, attached, and unreachable. `io.iops` hides `reads`, `writes`,
`discards` and `flushes` inside `ops`, and `io.merges` hides `read_merges` and
`write_merges` inside `merges`, in the same way.

One word on three entries:

```yaml
- id: io.bytes
  emits: [rate, summary, timeline, topk, raw]
- id: io.iops
  emits: [rate, summary, timeline, topk, raw]
- id: io.merges
  emits: [rate, summary, raw]
```

The alternative is the `count` → `raw` pairing the `cpu` thread added to the
mode × emit matrix for exactly this reason, recorded in §5 above as needing to
be promoted or reverted. Either closes io 7; this family is the second to want
it, which is the argument for promoting rather than reverting.

### To stream C, for `src/encode.rs` — the loss headline names three of five (io 9)

`Losses` counts `dropped`, `failed_reads`, `late_ticks`, `undecoded` and
`throttled`. The text renderer prints the first three, so a run whose only loss
is `undecoded` — this family's ordinary case, and how `/proc/diskstats` says a
line was left out of the total — announces `LOSS` and then three zeros, and the
reader has to infer the real count from how many reason lines follow. Every
structured format already carries all five, so this is the text renderer alone;
it is also the format in which someone is deciding whether to trust the numbers.

```
  LOSS: 0 dropped, 0 failed reads, 0 late ticks, 4 undecoded
```

Zero-valued categories could be left out entirely, which would read better
still; what cannot stand is a headline that names a fixed three of the five.
Fixing io 4 — a device appearing mid-run is now counted rather than only noted —
makes this more likely to bite rather than less.

### To stream C, for `src/emit.rs` — a series cannot be split by which counter it is (net 5)

`SeriesKey::of` is the first key of an event, and the key model has no variant
for "which counter this number is". `net.iface` carries errors, drops, fifo
overruns, collisions and carrier losses per interface; its measured value is
errors, because the catalog's caveat forbids summing a bad frame with a full
queue. The consequence is that `rate`, `timeline` and `summary` all answer about
errors, and a link losing every frame given to it reads `0 count/s`.

The collector cannot fix this. One event per counter kind, which is what the
shape wants, would key every one of them `iface:veth0` and collapse them back
into one summed series — the thing the caveat forbids. Writing the counter kind
into the interface key's `name` would make the key a lie and break every join on
it.

What would work, smallest first:

```rust
/// A sub-series within one key: which of several counters this number is.
/// Appended to the series key, so `iface:veth0` with `counter: "drops"`
/// becomes the series `iface:veth0/drops`.
pub struct Event {
    ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series: Option<String>,
}
```

with `SeriesKey::of` appending it when present. Every existing source leaves it
`None` and nothing changes for them. `net.backlog` wants the same thing for
drops against overflows, `io.bytes` for reads against writes (which would close
io 7 the other way), and finding 22 above — a NUMA node has no key — is a third
instance of the same shape. This stream has no standing to choose between this
and a key variant; the point is that the collector layer cannot.

Until then, `net.iface` states in the run's notes what its series is leaving
out, which is visible but is not a number anyone can aggregate.

### To stream C, for `src/emit.rs` — a timeline point cannot carry its error bar (gpu 1)

`raw` is the only emit that serialises `ts_err_ns`. `gpu.engine_busy` records a
one-second error on every event, because NVML averages occupancy over a window
it chooses, and does not declare `raw` — so the invariant in `CLAUDE.md` is
honoured in the event and lost in the emit. Four points 200 ms apart may all be
one driver sample and nothing in the output says so.

```rust
pub struct Point {
    pub offset_ns: u64,
    pub value: f64,
    /// How far the instant may be from the sample, when it is not zero. Carried
    /// per point rather than per series: a collection can mix a stamped event
    /// with a polled one.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub ts_err_ns: u32,
}
```

The same field belongs on a `SeriesSummary` as the largest error over the series,
for the same reason. Stream B declaring `raw` on `gpu.engine_busy` would also
close it, and is a smaller change; the two are alternatives.

Until then the collector states the error bar, with its size, as a degradation
of the run, which reaches every format that carries one.

### To stream B, for `catalog/net.yaml` — a caveat that is now true of two modes of three

`net.iface` and `net.backlog` both carry:

```
- "These are counters, so a run reports the increment between two reads and
   its first read is a baseline that produces nothing."
```

True of `poll` and `count`. Since net 3 was fixed it is not true of `snapshot`,
which reports the counters as they stand under `*_since_boot` names and takes no
closing read. Suggested:

```
- "These are counters. A `poll` or a `count` reports the increment between two
   reads, and its first read is a baseline that produces nothing; a `snapshot`
   observes no window of its own and reports the counters as they stand, under
   names that say so."
```

### To stream B, or to whoever takes §1 — modes are declared per entry, not per backend

Raised again from a third direction. `gpu.engine_busy` declares `count` because
its first backends are kernel PMUs, where engine occupancy is a busy-nanosecond
counter. The `vendor:nvml` fallback has no counter to difference: it reports a
level the driver averaged over a window it chose. A `count` served from it is an
integral of a sampled ratio, which is the arithmetic `gpu/device.rs` already
refuses by name for energy.

This stream wrote that refusal, ran it, and withdrew it — because a collector
refusing a declared mode at attach is the pattern io 1 and net 7 record, and the
family's own sweep exists to keep it out. The refusal belongs in the validator,
which needs `modes:` to be declarable per backend the way `keys:` and
`filter_fields:` already are. Until then `gpu.engine_busy --mode count` on NVML
answers with a window that is now correct and a number that gpu 3 says should
not exist.

---

## 34. A polling cadence anchored on boot, so a test passed or failed on uptime

Found by the coordinator while re-measuring the project's own numbers, after
`make check` came back red on a tree whose last six merges were all green.

`collect::mem::cadence::Cadence` separates two rates: the pipe behind the
preload shim has to be drained every ten milliseconds or it overflows, and the
series the request asked for is published on its own, coarser grid. `due()`
decides which ticks publish, and the grid was anchored at zero:

```rust
Self { interval_ns: interval.as_nanos() as u64, due_ns: 0 }
```

Zero in a `CLOCK_MONOTONIC` domain is boot. So the first slot of a `poll@1s`
grid was one second after the machine started, not one second after the run
started, and the slot the run actually landed on depended on how long the
machine had been up. `due()` also treats "within a tenth of an interval" as due,
which is right for a run whose tick is coarser than its grid and is what turned
a skew into a failure:

```
interval 3600 s, slack 360 s, uptime 3483 s
  first call:  due_ns := 3600 s        (the next slot since boot)
  second call: 3483 + 360 = 3843 s  >  3600 s  -> due again, immediately
```

Two unit tests of `mem.heap.live` assert that a second sample an hour apart
publishes nothing, and they **pass on a machine that has been up for more than
an hour and fail on one that has not**. That is the shape `STATE.md` warns about
in the other direction: a test asserting what the machine was doing. Here the
test was right and the code was wrong, and the machine decided which of them was
visible.

The grid now anchors on the first sample, which is the only moment that belongs
to the run:

```rust
let Some(due_ns) = self.due_ns else {
    self.due_ns = Some(now_ns.saturating_add(self.interval_ns));
    return true;
};
```

`the_grid_is_anchored_on_the_first_sample_rather_than_on_boot` asserts it with
the uptime that produced the failure, so it is deterministic on any machine.
The three existing cadence tests pass unchanged, because they all call `due(0)`
first and zero is now an anchor rather than a coincidence.

**The user-visible half is worth stating separately**, because the tests are not
the reason to care: a `poll@1s` run started at an uptime of 999.95 s published
its second point 50 ms after its first. Only `mem.heap.live` and
`mem.heap.retained` use `Cadence`, so that is the blast radius today — and it is
the layer every polling collector would use if the runner's per-request pacing
grows.

---

## 35. Six tests asserted what this machine can serve, not what the build has

The same red suite, two files further on. Both are `mem` scenarios about the
three refusals, and both chose a source that stopped qualifying when finding 15
was half fixed.

`a_source_this_build_does_not_collect_exits_three_and_says_what_is_missing` ran
`examples/mem-refused-not-built.yml`, which asked for `mem.reclaim` and expected
exit 3, *"this build has not written it"*. `the_three_refusals_are_told_apart`
used `mem.compaction` the same way. Both entries declare `bpf:tp` and nothing
else, and a classic tracepoint is attached by reading a numeric event id out of
tracefs — which uid 1000 cannot read. So the honest answer became exit 1,
*"this machine cannot serve it"*, and the scenarios failed against a tool that
had just become more truthful.

The premise was written into the session file in so many words: *"This machine
has the tracepoints, has `CAP_BPF`, and has the kernel for it."* It has the
tracepoints; it cannot read the file that names them.

A scenario about the *build's* gap has to pick a source whose backend this
machine genuinely serves, or the machine is a fourth answer standing in for the
third. Both now use the PEBS-or-IBS chain — `mem.latency` and `mem.sharing` —
which every x86 cpu serves through one of its two backends and which no build
has written. The session file records why it stopped being `mem.reclaim`, and
the day finding 9's catalog pass lands, `mem.reclaim` goes back to exit 3 on its
own without either scenario needing to change.

`tests/budget.rs::the_help_and_the_run_agree_that_a_budget_is_enforced` is the
same shape and is fixed the same way. It asserted that `io.latency --budget
0.1%` names what it would degrade onto — true where the chain's best backend
costs more than the ceiling, and not true here, where `io.latency` falls through
to `/proc/diskstats`, which is free and needs no plan. It now accepts either
honest answer and still refuses the retracted one.

### The same shape, three more times, and what it says about the class

`net` and `proc` each have a scenario that runs a session of unwritten sources
and requires exit 3. In `net` five of the eight are `bpf:tp`-only, so the
session was rebuilt out of the two `fentry` sources, whose requirement is BTF
and which therefore answer 3 anywhere BTF exists; the sweep over all eight now
requires each to give the answer that matches whose gap it is, and never the
third. In `proc` **all four** unwritten sources are `bpf:tp`-only, so this
machine cannot produce exit 3 for that family at all, and the scenario says so
in place of pretending: it takes either refusal, requires the matching sentence,
and names the catalog change that would settle it.

`scenario_power` failed differently and is the most interesting of the six. Its
sweep classifies every answer by exit code, and its constant for exit 1 was
still documented as *"usage, or a failure while running"*, with a note that
`BRIEF.md` asks for "cannot mean anything" and "this machine cannot serve it" to
be told apart and *"today they are not"*. They were told apart — stream A of the
fifth wave did exactly that, and "no viable backend" moved from 2 to 1. The
scenario never noticed, because every `power` source resolved on this machine
until the tracefs probe became honest and `power.throttle` stopped. So the test
was carrying a retracted claim in a doc comment and a matching assertion, and
only a change elsewhere made it visible.

**What the class has in common** is not "flaky tests". Each of the six encodes,
as an assertion, something that was true of this machine on the day it was
written: that tracefs could be probed loosely, that a chain would reach its
expensive backend, that exit 1 had one sentence. None of them is a test of the
tool that failed; every one is a test of the machine that succeeded for a while.
`STATE.md` already records the rule that catches this — *assert the separation
rather than a calibrated threshold* — and the six extend it: **where a tool has
several honest answers, a test must either assert all of them or state which
machine it is asserting.**

---

## 36. Finding 9, landed: twenty-seven chains, and what it moved

The catalog pass finding 9 asked for, done in one commit over `catalog/*.yaml`
and two probes in `src/capabilities.rs`. Recorded here rather than left as a
diff, because the numbers are the argument.

**Twenty-seven entries gained a `bpf:raw_tp` ahead of their `bpf:tp`**, with the
classic one behind declaring `equivalent: true` where it had no degradation of
its own, and keeping the degradation where it had one — `io.syscall` and
`mem.faults` both degrade for a reason that belongs to the tracepoint rather
than to the attach path, so the raw entry carries the same note and neither is
equivalent to anything.

Every tracepoint name was checked against this kernel's BTF before it was
written down, one by one, not by trusting the audit in finding 9:

```console
$ strings -n 6 /sys/kernel/btf/vmlinux | sed -n 's/^btf_trace_//p' | sort -u
```

All thirty explicit names are published. **Three entries were left alone**, and
each for its own reason:

| Entry | Why |
|---|---|
| `gpu.job` | `drm_run_job` and `drm_sched_process_job` are module tracepoints, absent from `vmlinux` BTF. The classic path is the only one, and this is the entry finding 9 already called out. |
| `io.fs` | Its requirement is the glob `tracepoint:ext4:*`, and a glob is not a name. Converting it would mean choosing which ext4 tracepoint the collector attaches to, which belongs to whoever writes it. |
| `io.readahead` | The same, for `tracepoint:filemap:*`. |

The two probes are the other half, from finding 15. `tracepoint:` under a
*readable* tracefs now calls `attach::classic_tracepoint_support`, which reads
the `id` file the attach reads rather than listing the event's directory; and
`kprobe` and `uprobe` are answered by `attach::probe_type_support`, which reads
the PMU type the kernel hands out at boot. Both existed, tested, and were called
by nothing.

### What it moved

```
before   100 sources: 62 usable here, 16 unverified, 22 out of reach
after    100 sources: 86 usable here, 10 unverified,  4 out of reach
                      67 of 202 backends have a collector
```

The `22 out of reach` was almost entirely this one gap: every source whose only
eBPF backend was a classic tracepoint, on a machine that cannot read tracefs —
which is the ordinary machine, not this one in particular. The six that became
`unverified` no longer are, because `uprobe` is now asked rather than assumed.

**The answers changed shape as well as count**, which is the part worth having.
`mem.reclaim`, `proc.exec` and the other twenty-five now answer *"no collector
is written for this yet"* — exit 3, this build's gap — where they answered *"no
viable backend on this machine"* — exit 1, and a reader sent to their kernel
configuration for an afternoon. Finding 35 records five tests that had encoded
the wrong one of those two as a property of the tool; all five were written to
accept either answer with the sentence that belongs to it, so all five passed
unchanged across this change, in both directions. That is what those rewrites
were for.

Three tests did fail, and each is the same lesson once more — a test naming the
shape of a chain rather than the property it is about:

- `catalog::a_fallback_that_neither_degrades_nor_declares_equivalence_is_refused`
  cleared `degraded` on `io.latency`'s second backend and expected a refusal.
  That backend used to be `fs:tracefs`; it is now a `bpf:tp` that declares
  itself equivalent, so clearing one of the two fields left the entry valid. It
  clears both now, which is what its name always claimed it did.
- Two `io` scenarios spelled `bpf:tp` into the message they expect — one for the
  backend that carries a `comm` filter, one for the backend nothing collects.
  Both now accept either id, because which of the two the resolver reaches for
  is a fact about the chain's order and not about the refusal.

### What is still waiting on mechanism, and is now the whole list

With this landed, **no source is blocked by an attach kind this build lacks
except through `fentry` and `usdt`**. `io.syscall`, `io.fs`, `io.fsync`,
`net.tcp.connect` and `net.socket` name `bpf:fentry` first; `runtime.gc`,
`runtime.alloc` and `runtime.custom` name `bpf:usdt`. Everything else on the
list of forty-one is a collector to write against a path that attaches today.

---
## 37. A distribution does not say that it is missing its own tail

Raised from outside the project, checked against it, and narrower than it
arrived -- the narrowing is the interesting half.

**Coordinated omission** is the bias that makes most published latency numbers
wrong: a measurement that misses readings misses *the slow ones*, because
whatever stalled the system is what stopped the measuring. A histogram built on
top reports a tail that is short by exactly the amount that mattered. It is
worth naming here because this project produces latency distributions and is
built on the rule that a number must never mean something other than what it
says.

### What is already closed, by construction

`poll` mode cannot produce `hist`. The mode × emit matrix refuses it, and every
polled source says so by name:

```console
$ omn net.tcp.rtt --system -m poll@1ms -e hist -d 1s
error: [net.tcp.rtt #1] mode `poll` cannot produce `hist`
  hint: in `poll` mode `net.tcp.rtt` emits raw, timeline, summary
```

That closes the largest opening there was, and it was closed for a different
reason: `CATALOG.md` §12 refuses it because *a histogram of a polled gauge is a
histogram of when the poller happened to look*, which is a property of the tick
and not of the program. The same sentence is the coordinated-omission argument,
written before anyone here used the term. A late tick in a `timeline` is
visible as a gap; a late tick folded into a bucket is not, and the tool does not
let anyone fold it.

### What is left open, and it is `sample` and `trace`

Both produce `hist`, and both lose events in the way that biases:

- **A sampler that is throttled** loses samples while the kernel declines to
  deliver, and `kernel.perf_event_max_sample_rate` is reached exactly when the
  event rate is highest. `Losses::throttled` counts the throttles, and
  `DECISIONS.md` already states the honest half of this -- *"how many events
  happened inside that while is not recorded anywhere"*, so a throttled run says
  it cannot tell how much it lost.
- **A traced source that drops** loses ring-buffer events under pressure, which
  is the busy period by definition. `Losses::dropped` counts them.

So the *losses* are counted and surfaced, which is this project's standing
invariant and it holds. What is missing is one step further: **the histogram
built from a lossy run is rendered exactly like one built from a complete run.**
A reader takes a p99 off it with nothing beside that number saying the
distribution is short, and the loss line -- which is printed, and is correct --
is several lines away and phrased as a count of events rather than as a
statement about the shape.

### What would close it

Not a new counter; the counters exist. The emit and render layers need to carry
the fact:

- A `hist`, `summary` or any percentile derived from a run with `dropped` or
  `throttled` above zero is marked, beside the value rather than above it, with
  what fraction of events is unaccounted for.
- The mark says which direction the bias runs, because it only runs one way: the
  missing events are the ones from the busiest moments, so **every percentile is
  a lower bound**, never an estimate.
- Where the count of missing events is unknown -- the throttled case -- it says
  that instead of a fraction, which is the answer `DECISIONS.md` already chose
  for the loss line.

**Belongs to whoever owns `src/emit.rs` and `src/render.rs`.** It is a defect of
the same class as the sixty-three: the tool telling a reader something that is
not true, in this case by omission. It does not block any collector, and it
should land before the phase 10 triage layer, because a layer that reads
percentiles to decide what to escalate is exactly who a short tail misleads.

---
## 38. BTF presence is not attachability, measured rather than assumed

Asked by the coordinator while six streams were writing collectors against the
twenty-seven `bpf:raw_tp` backends finding 36 added. Those were checked name by
name against `/sys/kernel/btf/vmlinux` and **not** against an attach, and the
difference between those two is the whole of finding 15: a requirement probed by
the easy question, a backend reported available, a resolver that selects it, and
a refusal that arrives with the target already launched.

If BTF presence and attachability diverge, every collector in the wave finds out
one at a time, at attach time. So it was worth asking once, of the whole
catalog, before they did.

### The answer

**Thirty-eight of thirty-eight attach.** Every name the catalog gives a
`bpf:raw_tp` backend, loaded with the two-instruction no-op `load_support` uses
and bound with `BPF_RAW_TRACEPOINT_OPEN`, on kernel 7.2.6 with `CAP_BPF` and
`CAP_PERFMON`. The cheap probe and the expensive one report the same
`86 usable here, 10 unverified, 4 out of reach`, and not merely the same totals —
the two listings are identical line for line.

### Why the probe stays cheap anyway

Because the expensive question costs what it costs:

```
omn list                    154 ms
omn list --verify-attach   1687 ms
```

Attaching and detaching a raw tracepoint is about thirty milliseconds of RCU
synchronisation, and a full listing probes thirty-eight distinct names. An
eleven-fold slowdown on the most-run command in the tool, to close a gap
measured at zero on this kernel, is a bad trade — and it would be paid by
everyone, on every listing, forever.

So `raw_tracepoint_support` still reads BTF, and the attach lives behind
`omn list --verify-attach`, which exists for the machine where the two *do*
disagree. That machine is by definition not this one, which is exactly why the
switch has to exist before anyone needs it.

### What makes "they agree" a claim rather than a belief

`tests/raw_tracepoints.rs` runs both listings and requires them to be identical.
It is an integration test and not a unit one, and the reason is worth stating
because it was tried the other way first: an in-process test binary is rebuilt
under a fresh hash every run and cannot carry file capabilities, so a unit
version of this sweep **skipped, and printed `ok`** — a test that could never
have run on the machine it was written for. `target/debug/omn` carries the
capabilities once `make` has applied them, so the integration version does the
attach for real; when it cannot, it says which case it is instead of passing
quietly.

The failure message names the fix rather than the symptom: if the two ever
disagree, `raw_tracepoint_support` should start calling
`raw_tracepoint_attaches`, which is kept beside it for that day.

### One thing that was tried and withdrawn

The probe was first changed to attach *always*, which is what `load_support`
two functions above already does and argues for in its own words — *"the probe
is a load, not a guess"*. It was withdrawn on the measurement above, and the
inconsistency is worth naming rather than hiding: **this layer now probes one
requirement by asking the kernel and another by reading a file, and the reason
is cost, not principle.** Should raw-tracepoint attach ever become cheap, or
should a machine be found where the two disagree, the cheap probe is the thing
to delete.

A second thing was withdrawn with it. Attaching turned the permission refusal
from *"`sched_waking` is available"* into *"attaching needs CAP_BPF"*, which
reads worse: it loses the fact that the tracepoint is there. The fix, kept in
`raw_tracepoint_attaches`, is to say both — *`sched_waking` is defined by this
kernel, and attaching to it needs CAP_BPF* — because "the tracepoint is not
there" and "you may not attach to it" send a reader to two different places.
## Needs from another stream

### From stream F, to the coordinator: finding 37 landed, and nine lines of it are in `src/encode.rs`

Finding 37 is closed. `src/emit.rs` computes a `Shortfall` and carries it on
`SeriesHist` and `SeriesSummary`, so the four structured formats carry it with
no encoder change at all. `docs/families/emit.md` is the write-up.

**The one thing outside the stream's file set.** The finding says the defect
"belongs to whoever owns `src/emit.rs` and `src/render.rs`", and `BRIEF.md`
gave stream F those two files. `src/render.rs` renders requests, listings and
refusals; it does not render results. `src/encode.rs` does, and `hist_text` is
in it. So the file list names the wrong second file, and the text rendering of
the mark — the format a reader actually takes a p99 off — is nine lines there:

- `bound()` and `comparison()`, two helpers returning `">="` or nothing;
- `>=` written into the `summary` and `hist` head lines in place of `=`, and
  beside `mean` and `max`;
- `push_shortfall()`, one line under each marked emit.

`src/render.rs` is untouched. No stream of the sixth wave owns `src/encode.rs`,
so this should merge without a conflict, but it is a file set overstep and is
declared rather than hidden.

### From stream F, to whoever takes it: `rate`, `topk` and `stack` are short in the same way and are not marked

A run that dropped events understates its `rate`, its `topk` totals and its
`stack` weights by exactly the same one-sided amount, and for the same reason:
the records a ring drops are the ones from the busiest moment. Finding 37,
`BRIEF.md` and `STATE.md` all scope themselves to `hist`, `summary` and
percentiles, so stream F marked those three and left the others alone rather
than widening a scope three documents had agreed.

The argument for widening it is that a reader of `topk` is choosing what to
look at next, which is the same decision a p99 is read for. The argument
against is that the shape is not the same: `Output::Stack` already carries
`unattributed` and `Output::TopK` already carries `Rest`, and each of those is
that emit's own statement about what it does not account for. A mark bolted
beside them would be a second such statement in different words.

`Shortfall::of` is public and takes `(&Losses, records, per_record)`, so
whoever takes this needs no new derivation — only a decision about how it
should read next to `Rest` and `unattributed`.
## Needs from another stream — the sixth wave, stream A (`io` through the block layer)

Five things the block-layer collectors could not do from their own paths.
Each is written out with the exact change wanted rather than applied in
passing, because three of the five are wanted by more than one family and the
second wave's lesson is that the coordinator lands those once.
`docs/families/io.md` has the reasoning behind all five.

### 1. In `src/bpf/insn.rs`: `bpf_get_current_comm` in the helper table

`insn::helper` lists the six helpers the scheduler programs call. A block
tracepoint hands over a `struct request` rather than a `task_struct`, so a
program that wants the submitting task's name cannot read it out of a field --
it has to ask the kernel for the current task's:

```rust
pub const GET_CURRENT_COMM: i32 = 16;
```

`src/bpf/io.rs` declares the number locally, with a comment saying where it
belongs, because `Insn::call` takes an `i32` and nothing forced the edit.
`proc` and `net` will both want it: every program attached to something that is
not a scheduler tracepoint has the same problem.

### 2. In `src/bpf/btf.rs`: a member offset that descends anonymous members

`Btf::member_offset` walks a struct's direct members and stops there. That is
enough for `task_struct`, `request`, `bdi_writeback` and `super_block`, and it
is not enough for `struct folio` or `struct page`, whose useful fields --
`mapping` and `index` -- live inside an anonymous struct inside an anonymous
union:

```c
struct folio {
    union {
        struct {
            memdesc_flags_t flags;
            union { struct list_head lru; ... };
            struct address_space *mapping;   /* wanted */
            union { unsigned long index; ... };
            ...
        };
        struct page page;
    };
    ...
};
```

Wanted: when a member's name is empty and its type is a struct or a union,
recurse into it and add its base offset. That is what the C compiler does with
the same declaration, so it is not an extension of the model, it is the model.

```rust
/// The byte offset of `member` within `struct_name`, descending into the
/// anonymous structs and unions the kernel uses to overlay one object on
/// another. An anonymous member has no name of its own, so its fields are
/// addressed as if they were the outer type's -- which is what C does.
pub fn member_offset(&self, struct_name: &str, member: &str) -> Result<i32, String>
```

**This is what blocks `io.pagecache`.** Its `bpf:raw_tp` requires
`mm_filemap_add_to_page_cache`, whose only argument is a `struct folio *`, and
the source's keys are `task` and `file` -- the file being `mapping->host->i_ino`
and `mapping->host->i_sb->s_dev`. Every step of that chain is reachable except
the first. `mem` will want the same thing for `mm_page_alloc`'s `struct page *`.

Hard-coding the offset was considered and refused: `folio.mapping` is at 24
bytes on this kernel because `memdesc_flags_t` is 8 and a `list_head` is 16, and
both of those have changed inside the last year. A baked-in offset is the
`bpf:kprobe` degradation -- "no BTF: struct layouts are not portable" -- applied
to a path that declares BTF as a requirement.

### 3. In `src/bpf/btf.rs`: the values of an enum, by name

`io.writeback` carries `enum wb_reason` as a bare number, because the enum has
gained and lost members between releases -- this kernel has no
`WB_REASON_LAPTOP_TIMER` and numbers everything after it one lower than a 5.x
kernel does. A name taken from a table compiled into `omn` is therefore a wrong
answer that reads exactly like a right one, which is the class of defect this
project exists to refuse.

The kernel publishes the answer in the same BTF everything else here comes from:

```rust
/// The name of `value` in `enum_name`, as the running kernel declares it.
///
/// For a field an event carries as the kernel's own number: the numbering is
/// a property of the kernel that is running, and a table compiled in here
/// would be a property of the kernel this was built against.
pub fn enum_name(&self, enum_name: &str, value: u32) -> Option<&str>
```

`sched` wants it for `enum wb_reason`'s neighbours and `net` for
`enum skb_drop_reason`, which is the field that makes `net.drops` worth having
at all and which grows every release.

### 4. In `catalog/io.yaml`: the glob that `io.readahead` requires

`BRIEF.md` gives stream A the decision and the catalog to stream nobody, so the
decision is here and the edit is the coordinator's. `io.readahead`'s first
backend is

```yaml
- {id: "bpf:tp", requires: ["tracepoint:filemap:*", "bpf"], cost: low}
```

and a glob is not a name: no probe covers it, so the source reports `?` under
`HERE` rather than yes or no, and the chain falls through to `bpf:fentry`, which
this layer does not have. The source is unreachable from both ends.

**The tracepoint to name is `mm_filemap_get_pages`**, and the entry wanted is a
`bpf:raw_tp` ahead of the existing pair:

```yaml
- id: io.readahead
  backends:
    - {id: "bpf:raw_tp",
       requires: ["bpf", "btf", "raw_tracepoint:mm_filemap_get_pages",
                  "raw_tracepoint:mm_filemap_add_to_page_cache", "kernel>=5.8"],
       cost: low}
    - {id: "bpf:tp", requires: ["tracepoint:filemap:mm_filemap_get_pages",
                                "tracepoint:filemap:mm_filemap_add_to_page_cache", "bpf"],
       cost: low, equivalent: true}
    - {id: "bpf:fentry", requires: ["btf", "bpf", "fentry:page_cache_ra_unbounded"],
       cost: low, equivalent: true}
```

Why that one, of the five the `filemap` subsystem publishes on this kernel.
Readahead effectiveness is *pages read ahead* against *pages used*, which is a
ratio of two counts and needs both:

| tracepoint | argument | what it would count |
|---|---|---|
| `mm_filemap_get_pages(mapping, start, end)` | `address_space *` | pages a read asked for |
| `mm_filemap_add_to_page_cache(folio)` | `folio *` | pages the cache had to fill |
| `mm_filemap_delete_from_page_cache(folio)` | `folio *` | pages evicted |
| `mm_filemap_fault(mapping, index)` | `address_space *` | a mapped page taken |
| `mm_filemap_map_pages(mapping, start, end)` | `address_space *` | a fault filled from cache |

`get_pages` is the denominator and `add_to_page_cache` the numerator, and no
single one of the five answers the question. Naming two in `requires` is not a
glob under another name: both are names, both are probed, and a kernel missing
either says which.

Note that it takes item 2 with it: both of the first two hand over a `folio` or
an `address_space` whose inode has to be reached, and `address_space.host` is a
direct member while `folio.mapping` is not.

`io.fs`'s glob -- `tracepoint:ext4:*` -- is deliberately **not** decided here.
Its first backend is `bpf:fentry` on `vfs_read`, which is the right answer and
is a mechanism this layer does not have; converting the filesystem-specific
fallback would be choosing which filesystem `io.fs` is about, and a machine with
btrfs and xfs mounted would get the ext4 answer or nothing. It waits for fentry.

### 5. In `src/event.rs`: a block key the plug's cached clock cannot collapse

Not blocking anything, and worth recording with its measurement before someone
rediscovers it.

`KeyValue::Blk { ptr, open_ts }` is the identity of one life of a request
pointer, and `open_ts` is `request.start_time_ns` -- deliberately the kernel's
own stamp rather than either collector's, because `io.queue` and `io.latency`
are two independently loaded program sets and a scope either of them
timestamped would differ between them.

The kernel takes that stamp with `blk_time_get_ns()`, which **caches its reading
for the lifetime of the submitting task's plug**. Every request created inside
one plug section therefore carries the same `start_time_ns`, and two lives of
one pointer inside one section are one key. Measured under eight concurrent
one-page `O_DIRECT` readers:

```
166448 events, 527 pointers, 165921 reuses, 5722 keys used twice
```

96.6 % of the reuses separated, where a bare pointer would have separated none.
The residual is a real hole: a completion that arrives after its pointer has
been reissued *within the same plug section* attaches to the wrong life, and
nothing says so.

Closing it needs a third component that both collectors can read and that
changes per request. `request.tag` is assigned at issue and cleared at
completion, so `io.queue` at the insert and `io.latency` at the issue read
different values; `internal_tag` is `BLK_MQ_NO_TAG` under the `none` scheduler,
which is the default. The sector would work for reads of distinct blocks and not
for a queue of rewrites. So this is a question about the key rather than about
the collector, and it belongs with whoever owns `src/event.rs` and `src/join.rs`
-- `gpu:fence_ctx+seqno` is the same shape and may want the same answer.

---
---

## Needs from another stream — the sixth wave, stream B (`net`)

Six requests, written with the signature wanted rather than as a description,
because the coordinator lands a shared change once and two streams asking for
the same thing in two shapes is how the third wave paid for the second.

**Two of them are blocking**, in the sense that three `net` sources shipped
without filter fields their catalog entries declare. They are the first two.

### To whoever owns `src/bpf/btf.rs`: a member lookup that walks into anonymous members

```rust
/// The byte offset of `member` within `struct_name`, including members of
/// anonymous structs and unions inside it.
///
/// A compile-time CO-RE relocation resolves `skb->protocol` by walking into the
/// anonymous union that holds it, adding each enclosing member's offset. This
/// does the same, so a program can read a field the kernel did not put in a
/// named structure.
pub fn member_offset(&self, struct_name: &str, member: &str) -> Result<i32, String>
```

The current implementation iterates the struct's direct members and compares
names. An anonymous member has an empty name and is skipped, and anonymous types
are not in `by_name` at all — `if name_off != 0` is the line that keeps them
out. So the recursion has to happen from the *parent*: when a member's name is
empty and its type is a struct or a union, search inside it and add the
enclosing offset.

What is unreachable without it, and what shipped instead:

| Field | Where it lives | Source | What it does instead |
|---|---|---|---|
| `skc_daddr`, `skc_dport` | anonymous struct in an anonymous union of `sock_common` | `net.tcp.lifetime`, `net.tcp.retransmit` | reports the local endpoint only; the catalog declares `daddr` and `dport` as filter fields and no event carries one |
| `skb->protocol` | anonymous union of `sk_buff` | `net.drops` | reports the protocol of the socket the drop belonged to, which a receive-side drop does not have |
| `skb->skb_iif`, `skb->dev` | the same union | `net.drops` | carries no interface at all |

`io` and `mem` will want this the moment either reads a field out of a `bio`, a
`page` or a `request`: the same pattern is everywhere in those structures.

The three collectors refuse to write the offsets down themselves, and the reason
is `docs/bpf-layer.md`'s own: an offset baked into a collector is the staleness
the whole layer exists to prevent, and the failure mode is a plausible number
read out of the wrong field on a kernel nobody tested.

### To whoever owns `src/bpf/btf.rs`: the names of an enum's members

```rust
/// The name this kernel gives `value` in `enum_name`, when it has one.
///
/// The parser already walks `KIND_ENUM` and `KIND_ENUM64` well enough to step
/// over them; what it does not do is read the `(name, value)` pairs that follow
/// the header, which are 8 bytes each for an enum and 12 for an enum64.
pub fn enum_variant(&self, enum_name: &str, value: i64) -> Option<&str>
```

`net.drops` reports `enum skb_drop_reason` as a bare number because of this. A
hundred and twenty-eight names the kernel publishes, a source whose whole point
is that it says *why* a packet was dropped, and a reader who sees `reason=3` and
has to go looking in `include/net/dropreason-core.h`.

A table of the names in the collector would close it today and was refused, for
the same reason as the request above: the values would be a constant no run
could check against the kernel it is running on, and the enum has been
reorganised at least once. A mislabelled drop reason does not read as wrong, it
reads as a different fault.

`proc.syscall` will want the same call for `__NR_*`, and `sched.irq` for
`softirq_to_name` if it does not read `/proc/softirqs` the way `net.softirq`
does.

### To whoever owns `src/bpf/insn.rs`: three helpers that are now written twice

`src/bpf/net_program.rs` holds near-duplicates of three private helpers in
`src/bpf/runqlat.rs`, and the duplication is not a style question: a program
built as instructions has no compiler to notice that two copies of a sequence
have drifted apart.

```rust
/// Reads `size` bytes of a kernel structure into the program's stack.
pub fn read_field(program: &mut Program, into: i16, size: i32, from: u8, field_offset: i32)

/// Adds one to a slot of a per-cpu drop counter, resuming at `resume` either
/// way: failing to record a loss is not a reason to stop recording
/// measurements.
pub fn count_drop(program: &mut Program, drops_fd: i32, drop_key: i16, slot: u32, resume: &'static str)

/// Computes the log2 bucket of `value_reg` into `r3`, in
/// `emit::BucketLayout::Log2`'s numbering. Six compares, thirty instructions,
/// no loop.
pub fn push_log2_bucket(program: &mut Program, value_reg: u8)
```

`push_log2_bucket` is the one that matters: it decides which bucket every
kernel-built histogram in this repository puts a measurement in, and two copies
that disagree by one report every duration in the neighbouring bucket — a factor
of two — with nothing anywhere saying so. Both copies carry an independent test
of the definition for that reason, which is a workaround rather than a fix.

Two more, smaller:

- `insn::helper` does not name `BPF_FUNC_get_current_comm`, which is 16, so
  `src/bpf/net_lifetime.rs` declares the number itself.
- `read_narrow` — clear a four-byte slot, then ask the kernel for exactly the
  field's own width — is how a `u16` or a `u8` is taken out of a structure with
  the loads this layer exposes. `net` has four fields that need it and every
  family reading a kernel structure will.

### To whoever owns `catalog/net.yaml`: four entries that offer what no backend can serve

Each is a line, and each is a promise a reader can ask for and not get.

1. `net.tcp.lifetime` and `net.tcp.retransmit` declare `pid`, `cgroup` and
   `container` targets. Both ends of a connection reach `TCP_ESTABLISHED` in
   softirq context and a retransmission is issued from a timer, so no task
   current at those moments is the connection's owner. The collectors refuse by
   name with exit 3, which is the right answer — attributing a connection to a
   process needs a socket-to-process map, which is a thing to build — but the
   entry should not offer it until there is one.
2. The same two declare `daddr` and `dport` as filter fields. Until the first
   request above lands, no event carries either and a predicate over one is
   false everywhere. `net.drops` declares `protocol`, which is carried but is
   `no-socket` for most drops.
3. `net.qdisc` declares `keys: [sock]`. A row aggregated over packets has no
   socket; what it has is the interface, which is what its events carry and what
   `--keys iface` should be able to ask for.
4. `net.qdisc` requires `raw_tracepoint:qdisc_dequeue` and needs
   `raw_tracepoint:qdisc_enqueue` as well — a wait is the gap between the two.
   Every kernel with one has the other, so nothing is broken today, but the
   probe would report the source available on a kernel that had only the
   dequeue. The collector refuses by name there.

### To whoever owns `src/emit.rs`: a ranking over an attribute, met a second time

This is already filed, by stream C of the fifth wave. It is recorded again
because `net.drops` is the strongest case for it in the repository: what a
reader wants ranked is the drop *reason* — which fault cost this machine the
most packets — and `topk` ranks series. A drop's series is its socket, which
most drops do not have, so `net.drops --emit topk` ranks one series called
`system`.

### To whoever owns `src/emit.rs` and `src/render.rs`: a note is not always a degradation

Every string a collector returns from `Collector::notes` is rendered as
`degraded: …`, because `emit.rs` appends them to the resolver's degradations.
The trait's own documentation says that is the contract — "what the collector
had to give up" — and the five `net` collectors were written to fit it.

What does not fit is a run stating a fact about itself that is not a
concession. `net.softirq` prints the kernel's own count of the handler runs it
bucketed, which is the strongest thing it can say about its own completeness,
and it comes out under `degraded:`. A second channel — `notes:` beside
`degraded:`, with the same plumbing — would let a collector say "here is what
another party counted" without it reading as an apology.
## Needs from another stream

Requests raised by a family stream against a file it does not own, with the
exact change wanted. The coordinator lands each one once — which is what wave
two learned and wave three paid for by not doing.

### `catalog/mem.yaml`: `mem.reclaim` requires half of the source it names — from `mem`

`mem.reclaim`'s chain requires `raw_tracepoint:mm_vmscan_direct_reclaim_begin`
and its `end`. Those fire when the *machine* has no free memory. A cgroup that
hits its own `memory.max` reclaims through `mm_vmscan_memcg_reclaim_begin` and
`mm_vmscan_memcg_reclaim_end` — different names, identical prototypes, same
`DECLARE_EVENT_CLASS` — and the entry declares `cgroup: bpf_only`, so the one
target the chain names is the one the required tracepoints cannot see.

Measured: a cgroup squeezed to a 32 MiB limit produced 1 439 reclaim episodes in
two seconds, **every one of them through the memcg pair**. A build attached only
to what the entry requires reports zero while the cgroup reclaims ninety
thousand pages, with nothing anywhere saying so.

The collector attaches both pairs and marks each row with its scope, so the
numbers are right today. What is wrong is the requirement, which will report
this backend available on a kernel that has only the global pair and then serve
a `--cgroup` request with a silent zero:

```yaml
- id: mem.reclaim
  backends:
    - {id: "bpf:raw_tp",
       requires: ["bpf", "btf",
                  "raw_tracepoint:mm_vmscan_direct_reclaim_begin",
                  "raw_tracepoint:mm_vmscan_direct_reclaim_end",
                  "raw_tracepoint:mm_vmscan_memcg_reclaim_begin",   # added
                  "raw_tracepoint:mm_vmscan_memcg_reclaim_end",     # added
                  "kernel>=5.8"], cost: low}
```

Adding them makes the probe strict: a kernel with only the global pair would
report the backend missing rather than serving half of it. That is the right
trade for an entry that offers a cgroup target, and it is a decision rather than
a task — which is why it is here rather than done.

### `catalog/mem.yaml`: `mem.compaction` requires a beginning and no ending — from `mem`

`unit: ns`, and the chain requires `raw_tracepoint:mm_compaction_begin` alone. A
duration needs both ends. On a kernel with the beginning and not the ending the
resolver calls the backend available and the collector refuses at attach, which
is the right refusal three layers after the one that could have said it.

```yaml
       requires: ["bpf", "btf", "raw_tracepoint:mm_compaction_begin",
                  "raw_tracepoint:mm_compaction_end", "kernel>=5.8"]
```

### `catalog/mem.yaml`: two entries declare a `mem` key nothing can carry — from `mem`

`mem.page_alloc` and `mem.kmalloc` declare `keys: [task, mem, ...]`, and
`mem:[base,len)` names one allocation — the address returned and the size given.
That identity is exactly what counting in the kernel discards, and counting in
the kernel is the only way either source is affordable at all.

`omn mem.kmalloc -k mem` therefore validates, resolves, and is refused at attach
with exit 3. The refusal names what to group by instead; what it cannot do is
stop the catalog offering the key. Either drop `mem` from those two entries, or
keep it against a future streaming backend and say what the aggregating one
degrades.

### `catalog/mem.yaml`: `direction` is not a filter field, so half of two sources cannot be events — from `mem`

`mem.page_alloc`'s summary is "kernel page allocations **and frees**" and
`mem.kmalloc`'s chain requires `kfree`. Both free tracepoints are attached, and
neither source can publish their events: a series is grouped by key, `direction`
is neither a key nor a declared filter field, and a histogram of allocations and
frees together is a histogram of neither. The totals are reported in each run's
notes instead.

Two ways to fix it, and the choice is the catalog's:

- add `direction` to `filter_fields` for both entries, so a reader can ask for
  one side and the default stops being a merge of two; or
- split each into two sources, which is what the separate emits and the separate
  units argue for.

Either way `mem.kmalloc`'s freed *bytes* stay unavailable: `kfree` carries the
pointer and the call site and nothing that says how large the object was, and a
collector that wanted the size would have to hold every live pointer in a map —
the per-allocation cost the aggregation exists to avoid.
## 38. What the `proc` event stream measured

The sixth wave's stream D, landing `proc.exec`, `proc.exit`, `proc.signals` and
`proc.syscall` over raw tracepoints. `docs/families/proc.md` has the family's
own account; what is here is what other families will meet.

### A machine-wide syscall tracer is affordable and a streaming one is not

Measured on the development machine, one second, every process:

```
31451 calls, reported as 5015 rows, for 26.9ms of cpu
   -- 1.81 % against the machine's own 1.49s of cpu in the same second
```

The same second through a streaming program is 31 451 ring records. The cost of
the measurement would grow with the workload, which is the shape `STATE.md`
warns about, and the numbers would begin describing a machine running `omn`.
`src/bpf/runqlat.rs`'s `(key, bucket) -> count` with an unrolled log2 is what
makes it 5 015 rows instead, and `emit::WEIGHT_ATTR` is what makes them count
correctly. **Stream C should read this before landing `mem.page_alloc`**: the
rate is the same order and the conclusion is the same one.

### The `bpf:tp` fallback is not a second road, and a refusal scenario should not depend on a probe

Every one of these four chains is `bpf:raw_tp` ahead of `bpf:tp` with
`equivalent: true`, and that is exact: the two see the same events and differ
only in what they need to attach. So there is no backend comparison to make, and
writing the classic collector would be writing a road nobody would drive.

What the unimplemented backend is *for* turned out to be class D. This family's
exit-3 scenario has been all three exit codes in three waves — 3 while a
`tracepoint:` requirement was probed by listing a directory, 1 once the probe
told the truth, and 3 again now that the raw path collects. The first two both
depended on what the machine could read. Pinning the unimplemented backend with
`backend: "force:bpf:tp"` takes the resolver out of the question entirely and
the answer is 3 on every machine, which is what a code meaning "this build does
not have it" should be. **Any family with an implemented and an unimplemented
backend in one chain can demonstrate exit 3 this way**, and several have been
asserting the machine instead.

### Five identical signals generate five and deliver one

`signal_generate` fires when a signal is generated, not when it is delivered,
and the tracepoint's own fifth argument says which. A non-real-time signal that
is already pending is not queued again, so `kill -USR1` five times over gives
five events of which one has `result=0`. The catalog summarises this source as
*"signals delivered"*, and the event count is not that number; `delivered` is.
Both are on the event.

This matters past `proc`. Any source counting a kernel event that the kernel may
then decline to act on has the same gap between "generated" and "happened", and
the tracepoint usually says which — `net.drops` has exactly this shape with its
drop reason, and stream B should check whether `kfree_skb`'s reason field is on
its events for the same reason.

### Parallel tests in one process cannot share an invented name

`docs/contributing.md` records that `cargo test` runs its tests in parallel
threads of one process, so a process-wide count cannot get an exact delta. The
same fact has a second face that cost time here: a test that invents a unique
name from `std::process::id()` invents the **same** name in every test of the
file, and each then counts the others' work as its own. Three machine-wide
scenarios reported 9 where they expected 5. The fix is a serial beside the pid;
the lesson is that "unique" in a parallel suite means unique per test, not per
process.

### Two shared-layer cost tests fail when another suite shares the machine

`collect::tests::what_a_run_reports_spending_is_the_cpu_it_actually_burned` and
`collect::tests::a_run_over_its_ceiling_with_nowhere_cheaper_to_go_stops_and_says_which`
both failed once during this stream and passed on every other run, with:

```
the collector was asked to spin for 200 ms and did not, so this measured nothing
```

The cause was another worktree's `make check` running at the same time on the
same machine, which is the ordinary condition during a parallel wave. This is
finding 11's class — a test asserting what the machine was doing — in
`src/collect/mod.rs`, which no family stream owns. It is worth the coordinator's
attention because a wave of six streams makes it reproducible rather than rare.

---

## Needs from another stream

Five requests from stream D of the sixth wave. All five are in the shared eBPF
layer or the catalog, none of them blocked this stream, and each is written with
the signature wanted so it can be landed once rather than five times.

### To whoever owns `src/bpf/insn.rs`: two helper numbers

```rust
pub mod helper {
    // ...
    /// `long bpf_get_current_comm(void *buf, u32 size_of_buf)`
    pub const GET_CURRENT_COMM: i32 = 16;
    /// `long bpf_probe_read_kernel_str(void *dst, u32 size, const void *src)`
    pub const PROBE_READ_KERNEL_STR: i32 = 115;
}
```

Both are needed by programs that cannot use what is already there.
`GET_CURRENT_COMM` because `sys_exit` hands its program a `pt_regs *` and a
return value and **no task at all**, so the caller's name cannot be read out of a
`task_struct` the way every other program in this layer reads it — and resolving
it from `/proc/<tid>/comm` at drain time answers for whoever holds that tid
*then*, which for a short-lived task is somebody else.
`PROBE_READ_KERNEL_STR` because `linux_binprm.filename` is a `const char *` and
a fixed-size `bpf_probe_read_kernel` over one walks past the end of whatever
allocation holds the name; the `_str` form stops at the terminator and zeroes
the whole destination on failure, which also satisfies this layer's rule that
every byte of a record is written before it is published.

Both are declared locally in `src/bpf/proc_events.rs` and
`src/bpf/proc_syscall.rs` meanwhile, in a module called `extra_helper` with the
reason written down, so the duplication is visible rather than quiet.

### To whoever owns `src/bpf/insn.rs`: three program fragments now written three times

Each of these exists in `runqlat.rs` and again in `proc_syscall.rs`, and two of
the three in `proc_events.rs` as well. They are not family-specific in any way:

```rust
/// Reads `size` bytes of a kernel structure onto the program's stack.
fn read_field(program: &mut Program, into: i16, size: i32, from: u8, field_offset: i32);

/// Adds one to a slot of a per-cpu drop counter, resuming at `resume` either
/// way -- failing to record a loss is not a reason to stop measuring.
fn count_drop(program: &mut Program, drops_fd: i32, drop_key: i16, slot: u32, resume: &'static str);

/// Computes `emit::BucketLayout::Log2`'s bucket index of `value_reg` into r3,
/// as an unrolled six-step binary search.
fn push_log2_bucket(program: &mut Program, value_reg: u8);
```

The third is the one that matters most: it has to agree with
`emit::BucketLayout::Log2` exactly, or every measurement lands in the
neighbouring bucket — a factor of two — and nothing anywhere says so. Two copies
of that agreement is two chances to break it, and streams A and C are both about
to write a third and a fourth. `runqlat.rs` and `proc_syscall.rs` each carry
their own test crossing the kernel's bucketing against an independent
expression of the definition; one shared fragment would need one.

### To whoever owns `src/bpf/map.rs` and `src/bpf/syscall.rs`: the capability advice belongs on map creation

Defect 8 of `docs/scenarios/proc.md`. `permission_advice` is attached to
`prog_load` in every program in this layer, and a program load is not the first
privileged thing a collector does — **map creation is**, and it fails first. So
what somebody without `CAP_BPF` actually sees is:

```
cannot create the `sched_process_exec` ring buffer: Operation not permitted (os error 1)
```

which names neither the capability nor the way to get it, and is what
`wakeup.rs`, `offcpu.rs`, `runqlat.rs` and `migrate.rs` have printed since wave
two. Nobody noticed because the message only appears on a machine without the
capabilities.

```rust
/// In `map.rs`, so that `Hash::create`, `Counters::create` and `map_create`
/// all get it:
fn map_failure(what: &str, error: std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        permission_advice(&format!("creating the {what}"))
    } else {
        format!("cannot create the {what}: {error}")
    }
}
```

Implemented locally in `src/bpf/proc_events.rs` and `src/bpf/proc_syscall.rs`;
the wrapper belongs one layer down, where every program would get it.

### To whoever owns `src/bpf/attach.rs` or `src/bpf/map.rs`: a way to narrow a program to a cgroup

The catalog marks `cgroup` and `container` as `bpf_only` on `proc.exec`,
`proc.exit`, `proc.signals` and `proc.syscall`, and `sched.wakeup`,
`sched.runqlat` and `sched.offcpu` before them. **No collector in this build
serves either**, and every one of them refuses by name — which is right, and is
the fifth wave of a declaration nothing implements.

Two mechanisms, and the choice is a decision rather than a task:

```rust
/// `u64 bpf_get_current_cgroup_id(void)` -- helper 80. One instruction, no map,
/// and the id is compared against one userspace resolved from the cgroup path
/// before the program was built. Reaches only the cgroup a task is *in*, not
/// its ancestors, so a request for a parent cgroup would miss its children's.
pub const GET_CURRENT_CGROUP_ID: i32 = 80;

/// `long bpf_current_task_under_cgroup(void *map, u32 index)` -- helper 37,
/// against a `BPF_MAP_TYPE_CGROUP_ARRAY` (type 12) holding the cgroup's own
/// directory fd. Answers the ancestor question correctly, and needs a map.
pub const CURRENT_TASK_UNDER_CGROUP: i32 = 37;
pub const BPF_MAP_TYPE_CGROUP_ARRAY: u32 = 12;
```

The second is the one that makes `--cgroup /sys/fs/cgroup/foo` mean what a
reader expects, since a container's tasks are usually in descendants of the path
they were given. It would also be what produces the `cgroup` **key**, which four
entries declare and nothing emits.

### To whoever owns `catalog/*.yaml`: three key declarations nothing emits

Defect 9 of `docs/scenarios/proc.md`, and defect 5 exactly, in three new places:

| Entry | Declares | Events carry | What to do |
|---|---|---|---|
| `proc.syscall` | `keys: [task, req, file]` | `task` | drop `req` and `file` |
| `proc.exec` | `keys: [task, cgroup]` | `task` | drop `cgroup` until the helper above lands |
| `proc.exit` | `keys: [task, cgroup]` | `task` | the same |

The validator accepts `--keys cgroup` and tells a reader that the source carries
it; a join written on it matches nothing and reports that it attributed zero
events, which reads as a workload that did nothing rather than as a key that was
never there. `req` and `file` on `proc.syscall` look like the remains of a
design where one event was one syscall with its arguments decoded; this
collector aggregates in the kernel because its own caveat says to, and cannot
carry either.

### To whoever owns `src/emit.rs` and `src/encode.rs`: a note is not a degradation

Defect 7 of `docs/scenarios/proc.md`. `Collector::notes()` is folded into
`Results::degradations` by `src/emit.rs` and printed as `degraded: <note>`, so a
backend that fell back to a lesser mechanism and a collector stating a fact
about its own measurement read identically. `sched.runqlat` has had the same
confusion since wave three.

What is wanted is two lists rather than one — a degradation is something the run
had to give up, a note is something the reader has to know — printed under
different words and serialised under different keys. Meanwhile every note in
this family is worded so that it survives being called a degradation, and a
wording chosen to survive its own label is a wording that will drift.

**Stream F should know about this one**, since it is adding the mark that says a
distribution is short: that mark is neither a degradation nor a note, and the
distinction it needs is the one this asks for.
*Written by stream E of the sixth wave, which owns `src/bpf/sched*.rs`,
`src/collect/sched/**` and `src/collect/gpu/**`. Six things four collectors
could not reach, each with the signature that would settle it.
`docs/families/sched.md` and `docs/families/gpu.md` record what was done on
this side of each.*

### To whoever owns `src/bpf/insn.rs` — two helpers the shared list does not name

`insn::helper` lists nine of the kernel's `bpf_func_id` values. Three of the
four collectors this wave landed need two more, and they are named locally in
`src/bpf/sched_common.rs::extra_helper` rather than added to a file five
streams write into:

```rust
pub const GET_CURRENT_COMM: i32 = 16;          // bpf_get_current_comm(buf, size)
pub const PROBE_READ_KERNEL_STR: i32 = 115;    // bpf_probe_read_kernel_str(dst, size, unsafe_ptr)
```

The second is not interchangeable with `PROBE_READ_KERNEL`: it stops at the
terminator, where a fixed span past the end of a kernel string can fault. Note
for whoever moves them — it does not zero the rest of the span, so the
destination has to be cleared first or the map carries this program's own stack
contents to userspace. `src/bpf/sched_irq.rs` does that and has a test for it.

### To whoever owns `src/bpf/insn.rs` — there is no bitwise `and`

`Insn` has `rsh64_imm` and `jset_imm` and no `and64_imm`. `sched.futex` has to
mask two flag bits off a syscall argument the way the kernel does, and without
an `and` it strips them by conditional subtraction: correct, tested, and eight
instructions where two would do.

```rust
/// `dst &= imm`
pub fn and64_imm(dst: u8, imm: i32) -> Self
```

Anything masking a field out of a register will want it. The four-byte load
trick — write the register to the stack and read back with `BPF_W` — covers
truncation to 32 bits and nothing narrower.

### To whoever owns `src/bpf/attach.rs` — the tracefs root and the `format` file

`attach.rs` knows where tracefs may be mounted and how to read a tracepoint's
numeric id, and publishes neither the root nor a way to read the `format` file
beside it. `gpu.job` is the only collector in this build on the classic
tracepoint path and it needs the field layout, so `src/bpf/sched_drm.rs`
carries a second copy of the two mount paths and a `format` parser of its own.
Whoever takes the next classic tracepoint will write a third.

```rust
pub fn tracefs_root() -> Option<PathBuf>;

/// Every field of `<category>:<name>`'s record, by name, with its offset, its
/// width and whether it is a `__data_loc`.
pub fn tracepoint_layout(category: &str, name: &str) -> Result<BTreeMap<String, Field>, String>;
```

`sched_drm.rs::parse_layout` is the implementation, with the `format` text of
two real kernels as its fixtures; moving it needs no rewriting.

### To whoever owns `src/bpf/` — a bounded read at an offset the verifier has not seen

A classic tracepoint's `__string` fields arrive as `__data_loc`: a `u32` at a
fixed offset holding a packed `(offset, length)` pair, with the characters
somewhere else in the record. Reading them means a read at an offset computed
at run time, and `PTR_TO_CTX` does not take variable arithmetic.

`gpu.job` wants one for the DRM ring's name and does without it, carrying the
scheduler kernel thread's name instead and saying so on every run. Nothing else
in this build has met a `__data_loc` yet; `io.fs` and `io.readahead` will, if
they take the classic path for their glob.

### To stream B, for `catalog/sched.yaml` — `sched.irq` promises the soft half

The entry's `summary` is *"Time spent in hard and soft interrupt handlers"* and
its `requires` are `raw_tracepoint:irq_handler_entry` and
`raw_tracepoint:irq_handler_exit`, which are the hard ones. Soft interrupt time
— the network receive path, block completions, the timer wheel — is
`softirq_entry` and `softirq_exit`, and this build does not attach to them:
attaching to a tracepoint the resolver never probed is how a backend comes to
be reported available and then fail, which is finding 15.

Both exist in this kernel's BTF, checked by name. The entry needs them in its
requirements before a collector may read them:

```yaml
- id: "bpf:raw_tp"
  requires: ["bpf", "btf",
             "raw_tracepoint:irq_handler_entry", "raw_tracepoint:irq_handler_exit",
             "raw_tracepoint:softirq_entry", "raw_tracepoint:softirq_exit",
             "kernel>=5.8"]
```

Until then every run of `sched.irq` says which half it measured, in its own
output rather than in documentation nobody reads at the moment it matters.

### To whoever owns `src/bpf/` and `src/collect/mod.rs` — no `sched` source serves a cgroup

`catalog/sched.yaml` declares `cgroup: bpf_only` on `sched.wakeup`,
`.runqlat`, `.offcpu`, `.migrations`, `.lock` and `.futex`. All six collectors
refuse a cgroup target by name. This predates the sixth wave; the four
collectors added in it inherited the refusal rather than each inventing a
different answer to the same gap.

What it needs is one helper and one comparison:

```rust
pub const GET_CURRENT_CGROUP_ID: i32 = 80;   // bpf_get_current_cgroup_id()
```

plus a way to turn a `Target::Cgroup(path)` into the id that helper returns —
`name_to_handle_at` on the cgroup directory, whose handle's first eight bytes
are the cgroup id. That second half is not eBPF and belongs wherever
`Target::Cgroup` is already resolved. Six entries stop being aspirational the
day both land.
## 39. What the sixth wave's merge exposed

Three things, which is what a merge of six streams is for. Two were the streams'
and one was the merge's own.

### The shared eBPF module was an append seam nobody had named

`src/bpf/mod.rs` was edited by five of the six streams, and the brief did not
forbid it — it named `attach.rs`, `insn.rs`, `map.rs`, `btf.rs` and `syscall.rs`
as the shared layer and omitted `mod.rs`, which is where a new program registers
its `mod` and its `pub use`. Five streams therefore added lines to one file.

**It cost nothing**, because every edit was an append and none touched another's
lines: four of the five auto-merged and the fifth was one hunk resolved by
keeping both. That is the "registries are append seams" rule holding, and it is
the third wave in a row where it has. What it says for the next brief is that
the seam should be named rather than discovered: either `mod.rs` joins the
forbidden list and the coordinator lands the registrations, or it is declared an
append-only file and streams are told to add at the end.

`docs/open-findings.md` was the same shape at six streams out of six, and that
one *was* expected: the brief sends every "needs from another stream" there. Six
appends to one file is six conflicts, all resolved by keeping both.

### `gpu.mem` lost a reading to a race it should have retried

Found by `the_same_source_polled_at_a_sane_interval_keeps_up_with_itself`, which
asserts a clean run and got `failed_reads: 1` with the reason attached:

```
gpu.mem on NVIDIA GeForce RTX 2080 Ti: Insufficient Size (NVML code 7)
```

Listing a device's processes takes two NVML calls — one for how many, one for
the list — and **they race**. A process that starts using the GPU between them
makes the second answer `NVML_ERROR_INSUFFICIENT_SIZE` with the count raised to
what it now needs. Under a suite of thirteen hundred tests sharing one board
that happened about once a run, and it was reported as a failed read, which is
exactly what it was: a reading the collector did not take.

It was not the scenario's fault and the scenario's zero was right. `process_list`
now retries with the size the library just asked for — the number it computed,
not a guess — up to four attempts, growing by one each time so the next arrival
does not cost another round. A device whose list is genuinely churning faster
than it can be read still reports a failed read rather than spinning.

This is the second NVIDIA-side race this project has found by running its own
suite against a shared board; finding 11 has the first.

### A `gpu` scenario that was late, not broken, asserted it twice

`a_poll_faster_than_the_driver_answers_counts_every_late_tick_and_says_so`
required `failed_reads == 0` while holding the *shared* lock, so it ran beside
the rest of the suite and beside whatever else was on the device. Its subject is
that lateness is told apart from breakage; requiring exactly zero failures made
it assert the machine as well, which is finding 35's class. It now requires
failures to be negligible against lateness, with the reason written down.

Its sibling keeps its `failed_reads == 0`, and that zero is now earned rather
than hoped for: the race above is retried, so a failure that survives is a
driver refusing, which is what the line is for.

### And one that was not a defect at all

`what_this_machine_cannot_check_about_gpu_job_is_named_with_its_numbers` failed
once during the merge with a refusal naming `CAP_BPF`, which on this machine is
never the reason `gpu.job` cannot run — the NVIDIA driver publishes no
`gpu_scheduler` tracepoints. The test was right and the binary was wrong:
`cargo test` had relinked `target/debug/omn` after a source change, and **a
relink drops the file capabilities**. `make` reapplies them and it passed.

That is the trap `CLAUDE.md` opens with, caught by a test rather than by a
person, on a machine where it has been caught three times before. Worth
recording as the one case where the suite noticed on its own.

---
## 40. A survey of other tools, triaged against what is already here

Raised from outside, and worth recording for the two suggestions that turned out
to be already solved — because both were solved *better* than the suggestion,
and the reasons are the interesting part.

### Rejected: per-microarchitecture event group tables

The suggestion was that `cpu.topdown` names a method without saying how to
compute it on Zen 4 against Sapphire Rapids, and that a table mapping forty
microarchitectures to raw counters and derived formulas — LIKWID's `groups/`
directory is the worked example — is the missing layer.

It is not missing; it is delegated. The entry requires `pmu:slots`,
`pmu:topdown-retiring`, `pmu:topdown-bad-spec`, `pmu:topdown-fe-bound` and
`pmu:topdown-be-bound`, which are **the kernel's own event aliases**, published
per machine under `/sys/bus/event_source/devices/cpu/events`. The mapping from
microarchitecture to raw counter is the kernel's, maintained by the people who
add each new part, and this catalog reads it rather than reproducing it. The
entry's own caveat has said so since it was written: *"map per CPU family, never
hard-code event names."*

What that buys is visible on this machine, a Zen 2 that has no topdown at all:

```console
$ omn list cpu.topdown
  here          no backend is usable on this machine
    1. pmu                    cost low    collector written
       needs    pmu:slots     missing: this machine publishes no `slots`
                              performance unit under /sys/bus/event_source/devices
```

A table would have had to know that Zen 2 lacks it. The kernel already does.
Maintaining forty microarchitectures is a cost this project does not pay, and
the licence of the best available table (GPL-3) is a second reason not to look
closely at it.

### Rejected: a privileged access daemon

The suggestion was LIKWID's model — a small setuid daemon holding the
privileges, rather than the whole tool running as root.

`DECISIONS.md` settled this and the reason still holds: `CAP_BPF` +
`CAP_PERFMON` on the binary, *"an unprivileged run is refused rather than served
by a backend that answers a different question. This keeps one chain shape to
reason about and to test."* A daemon is a third privilege shape, not a second,
and it makes what a measurement can reach depend on how the machine was set up
rather than on what the machine is. The project already refused that trade once
by name.

It is worth noting that the tool is *not* all-or-nothing today, which is what
the suggestion was really about: two file capabilities, granted to one binary,
are already the narrow middle it was reaching for.

### Taken: three things that were genuinely absent

**An `msr:` probe.** `power.freq`'s `pmu` fallback requires `msr:aperf` and
`msr:mperf` and both read `unverified: no probe is written`, which the resolver
treats as a reason to prefer a backend it could check. Now probed, and the
answer on this machine is the useful one — the device exists and is not
readable:

```
needs msr:aperf   missing: /dev/cpu/0/msr is present and not readable by uid 1000:
                  an MSR read needs root or CAP_SYS_RAWIO, which is more than this
                  project asks for elsewhere
```

The register's name is deliberately not checked against anything: there is no
enumeration of which MSRs a part implements, and reading an absent one answers
zero rather than failing — which is exactly why a collector must never guess.

**Cache state between benchmark runs.** Phase 8's environment control listed the
governor, the clocks, THP, affinity and throttling, and said nothing about what
the previous run left in the last level cache or in a NUMA node's memory. This
project's own `mem` scenarios turn that effect into a measurement on purpose — a
shuffled walk over 64 MiB against one over 4 KiB — so inheriting it by accident
between repetitions is measuring the order the runs happened to go in. Added to
the protocol beside dropping the page cache.

**Prefetcher state in the environment capture.** Captured, never changed:
turning the prefetchers off is a real lever and a modification of the target,
which boundary 5 puts outside this tool. Two runs on one machine with different
settings are not comparable and nothing would have said why.

### Left open, because it contradicts something already written

Dynamic binary instrumentation. `Cost::Extreme` is documented as *"replay or
dynamic binary instrumentation"* while the accepted boundaries call DBI
*"outside the kernel-level core"* — a band reserved for a mechanism the
boundaries exclude. `DECISIONS.md` now carries it as an open line with the three
things that make it unlike any other backend: it is exclusive with the machine,
its clock is a different domain rather than an uncertain one, and it would need
a `Determinism` the enum refuses to have.

---

---

## 41. The sixth wave's requests, landed once — stream S of the seventh

The five family streams of the sixth wave wrote roughly twenty requests under
`## Needs from another stream` rather than reaching into files they did not own.
None had been landed when the seventh wave was dispatched, and three of them had
been asked for twice by two families independently, which is what a coordinator
stream exists for.

`docs/families/shared-layer.md` is the full record: what was landed, what turned
out to be something else, and what was left with the reason. In short:

- **`Btf::member_offset` descends anonymous members** (asked by `io` and `net`),
  which unblocks `io.pagecache` and makes `skc_daddr`, `skc_dport`,
  `skb->protocol` and `skb->skb_iif` reachable for the first time.
- **`Btf::enum_variant`** (asked by `io` and `net`) reads the kernel's own
  `(name, value)` pairs, so a drop reason can be named by the kernel that is
  running rather than by a table compiled in here.
- **`insn::helper` names `GET_CURRENT_COMM` and `PROBE_READ_KERNEL_STR`**, which
  four families had declared locally, and **`Insn::and64_imm` exists**.
- **Five copies of `push_log2_bucket`, eight of `read_field`, six of
  `count_drop` and four of `bucket_low_ns` became one each**, with one test
  where there were four. The labels stay with the caller, because a label is
  defined once per instruction stream.
- **The capability advice moved onto map creation**, where the first privileged
  failure actually happens, across 51 sites in 18 files.
- **`attach::tracefs_root` and `attach::tracepoint_layout`** are published, so
  the classic tracepoint path has one `format` parser rather than two.
- **Eight catalog entries stopped promising what nothing can serve**, and
  `io.readahead`'s glob became two names — the one change that moved the
  numbers, from 86 usable on this machine to 87.

Two things worth carrying out of it.

**The refusals moved a layer earlier, and two scenarios had asserted the old
one.** `mem.kmalloc -k mem` was refused by the collector at attach (exit 3) and
is now refused by the validator (exit 2), because the entry no longer declares a
key nothing can carry. The same for `net.tcp.lifetime --pid`. Both scenarios now
assert the earlier refusal, and the `net` one needed a different source for its
third case entirely — `net.socket`, which resolves onto `bpf:fentry` and has no
collector — because "a target the catalog offers and this build cannot serve" is
no longer true of `net.tcp.lifetime`. A shipped session, `examples/web-latency.yml`,
stopped validating for the same reason and is now written against what the
source serves.

**Two assumptions were corrected by the kernel, both inside the tests written
for this change.** `sock_common.skc_daddr` is at offset zero, so an "overlaid
fields are not at zero" assertion would have been asserting the layout rather
than the lookup. And value zero of `enum skb_drop_reason` is `SKB_NOT_DROPPED_YET`,
not the `SKB_DROP_REASON_NOT_SPECIFIED` the test expected — the guess was a
name, it was wrong, and it would have been wrong silently in every rendered drop
table. That is the argument for reading the kernel's table, made by its own test
before the feature had a caller.

### `docs/scenarios/io.md` records that `scenario_io` never flaps, and it does

Measured while this stream ran, with four agents on the machine and a load
average near 5 on 24 cores: two failures in three runs of `scenario_io` alone,
both within about 1 % of a threshold. The same file on the tree *before* this
stream's changes failed one run in three, and that one was not a margin —
`0 bytes traced for the 16777216 the workload read`. The flakiness is older than
this stream and worse than the document says, and a total miss is a different
fact from a 1 % shortfall. It belongs to whoever owns `io`.
## Needs from another stream — the seventh wave, stream P1 (memory, end to end)

Five requests, each with the defect behind it in `docs/scenarios/cross-memory.md`
and, where one exists, the test in `tests/cross_memory.rs` that pins today's
behaviour and says what to change when it moves. None blocks the crossing: it
is landed in two sessions, with the `TEMPORAL` half done as arithmetic in the
test and labelled as such. Each one would move a line of that document from
prose into the tool's own output.

### To whoever owns `src/join.rs`: say `TEMPORAL` for crediting within a key by recency (D1)

`eq` on `task` from `mem.heap.alloc` to `mem.rss` prints `evidence KEY` and lists
one row per allocation. Every allocator call replaces the one open state for
the task (`peak_open: 1`, `replaced` in the thousands), so each reading goes to
the call that came last. `KEY` is right for the association, and
`tests/join_engine.rs` rightly holds `proc.threads -> proc.fd` to it. It is
wrong for the rows: which replaced state got a reading was decided by time.

Wanted, with no change to `evidence` itself:

```rust
pub struct Correlation {
    // ...
    /// How a downstream event was credited to one state among several that
    /// held the same identity in turn. `None` when no state was replaced, so
    /// the association and the crediting are the same claim. `Some(Temporal)`
    /// when states were replaced: the key says which identity, the clock said
    /// which of its states.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within_key: Option<Evidence>,
}
```

set in `assemble` from `states.replaced > 0` for `eq` and `scoped_eq` (not for
`interval`, whose replaced ranges are an address handed back out, and not for
`wakeup`). The text encoder would print it beside the evidence, for instance
`evidence KEY, TEMPORAL within task`. The pinning test only requires the word
`TEMPORAL` to appear somewhere in the correlation, so any shape will do.

### To whoever owns `src/join.rs` and `src/catalog.rs`: a match kind that is `TEMPORAL` by construction

The half of this crossing that puts a resident size beside the ranges is done in
`tests/cross_memory.rs`, because no match kind can express it. It is always the
same computation: a polled total's change between two readings, credited to the
states that opened or closed between them.

```rust
// catalog.rs
pub enum MatchKind { Interval, Eq, ScopedEq, Wakeup, Between }

// join.rs
/// `between`: each `to` reading's change since the previous reading with the
/// same `via` key is credited to the `from` states that opened or closed in
/// that interval. Always `Evidence::Temporal`; `check` refuses it when `to` is
/// not a `poll` or `snapshot` source, since a traced event has no previous
/// reading to differ from.
fn between(rule: &Rule, events: &[&Event]) -> Result<Correlation, JoinError>;
```

`via: task` for this crossing. With it, the second column of the first table in
`docs/scenarios/cross-memory.md` becomes a join line with `evidence TEMPORAL` on
it, rather than a table in a document.

### To whoever owns `catalog/mem.yaml`: three entries declare a `mem` key no collector emits (D2)

| Entry | Declares | Events carry |
|---|---|---|
| `mem.rss` | `keys: [task, cgroup, mem]` | `task` |
| `mem.heap.live` | `keys: [task, mem]` | `task` |
| `mem.heap.retained` | `keys: [task, mem]` | `task` |

This is the sixth wave's `proc.syscall` finding in three more places, and here
it costs a whole run: an `interval` join on `mem` onto `mem.rss` validates,
measures, and exits 1 at the join with nothing printed. Drop `mem` from
`mem.rss`. For the two heap entries, drop it unless the next request lands, in
which case it becomes true. `examples/mem-fragmentation.yml` asks for `mem` on
both `mem.heap.retained` and `mem.rss`, and would need the line changed.

### To whoever owns `src/collect/mem/heap/`: per-range residency of live allocations

The one `KEY` this crossing is missing: "the resident pages an allocation still
holds at the end". `retained.rs` already scans `/proc/[pid]/pagemap` over the
freed ranges. The same scan over `LiveSet` published per range would make it a
measurement:

```rust
// one event per live range at each poll of mem.heap.live, under emit: raw
Event {
    kind: "mem.heap.live",
    keys: vec![KeyValue::task(pid), KeyValue::Mem { base, len }],
    attrs: vec![Attr::uint("bytes", len), Attr::uint("resident", pages_present * page)],
    ..
}
```

An `eq` on `mem` from `mem.heap.alloc` would then join each allocation to its
own residency at `KEY`, and D2's heap rows would be true rather than dropped.
The cost is the scan, which is bounded by the live set, and should be declared
on the backend rather than discovered.

### To whoever owns `src/collect/mod.rs` and `src/collect/mem/heap/mod.rs`: one shim stream per launch (D3)

`mem.heap.alloc` and `mem.heap.retained` on one launched program are refused
with `two collectors disagree about OMN_HEAP_FD` and **exit 2**.

- `CollectError::ConflictingEnvironment` is classed `Whose::Request`. The
  request validates and means something; the build cannot serve it. Per
  `DECISIONS.md` ("Valid, and not built is exit 3") it should be
  `Whose::Build`, whichever collectors disagree.
- The real fix is for every preload collector of one launch to share one pipe:
  a `HeapPrepared` for a launch that already has one reuses its `write_fd` and
  fans the drained records out to each collector. Then the crossing is one
  session and not two, and the decomposition can be joined to the ranges it
  decomposes.

### To stream E for `src/encode.rs`, stream S for `catalog/mem.yaml`, and the coordinator for `DECISIONS.md`: the mTHP explanation is contradicted by measurement (D4)

The note under every interval join (`src/encode.rs:231`), the `mem.faults`
caveat, `DECISIONS.md` "What the measurement taught" and
`docs/scenarios/mem.md` all explain flat fault counts with *multi-size* THP, as
in "the kernel picks larger folios for larger regions". On the development
machine every multi-size size is `never`, and the counts are predicted exactly,
21 of 21 at seven sizes, by one fault per aligned 2 MiB page a mapping wholly
covers plus one per 4 KiB page outside them. An 8 MiB mapping costs 515 faults,
or 4 when it lands on four aligned huge pages, which it did on five runs of
twelve. The table is in `docs/scenarios/cross-memory.md`. What is true on every
kernel is what the note should say: fault counts depend on how a range is
backed and on where it lands, so two ranges of one size can differ by two
orders of magnitude; the THP mode is in the environment. The first half of the
note, "EVENTS counts faults, not bytes", is right and matters most.

---
## Needs from another stream — stream P2, the compute crossing

Written by the stream that put `cpu.profile`, `sched.runqlat` and
`sched.offcpu` in one table for one task. Every request below is backed by a
measurement in `docs/scenarios/cross-compute.md`, whose finding number is given,
and none of them was worked around in a file this stream does not own. In the
order they are worth doing.

### To whoever owns `src/bpf/offcpu.rs` and `src/collect/sched/offcpu.rs`: split the queue wait out of a block — finding 1

A block is timed from the switch that took the task off the cpu to the switch
that gave it one back, so the run queue wait after the wakeup is inside
`total_ns` — and inside `sched.runqlat` as well. Measured: 100 sleeps of 5 ms
behind a spinning shell report 582 ms off cpu, of which 77 ms is the kernel's
own queue wait, which `sched.runqlat` reports too. Any decomposition that adds
the two counts it twice, and nothing today says how much of the runnable row
that is.

The program already has both switches; what it lacks is the wakeup between
them, which `src/bpf/runqlat.rs` already hooks. Wanted, per row:

```rust
// in the sched.offcpu event, beside what is there now
Attr::uint("total_ns", switched_in - switched_out),   // unchanged, so nothing breaks
Attr::uint("queued_ns", switched_in - woken),         // new: the part that was runnable
```

with `woken` written into the in-flight entry by a `sched_wakeup` program, and
`queued_ns = 0` for a block whose wakeup the run did not see (say so in a
note). `total_ns - queued_ns` is then the blocked row, and the catalog caveat
*"blocked, not preempted"* can add *"and the wait after the wakeup is
`queued_ns`"*.

### To whoever owns `src/collect/mod.rs` and `src/collect/launch.rs`: the launched program's lifetime — finding 4

The whole of a decomposition is the task's lifetime, and no output carries it.
`observed_ns` runs from the release to the last drain and was 14 to 146 ms past
the workload's own clock here, so every residue is a range and a double count
smaller than that cannot be proven. `Gated` sees both ends already.

```rust
// in Collection, and so in every result
#[serde(default, skip_serializing_if = "Option::is_none")]
pub lifetime: Option<Lifetime>,

pub struct Lifetime {
    /// `CLOCK_MONOTONIC` at the write that released the gate.
    pub released_ns: u64,
    /// When the reap saw it, with how late the reap could have been.
    pub exited_ns: u64,
    pub exited_err_ns: u64,
}
```

`exited_err_ns` is the runner's tick unless the exit is taken from a pidfd
becoming readable, which would make it the wakeup latency instead. Either is
honest if it is carried.

### To whoever owns `src/join.rs`, `src/request.rs` and `src/encode.rs`: a decomposition beside the joins — the whole crossing

Today the arithmetic is in `tests/cross_compute.rs`, and a session prints three
sources that do not add up. Wanted, in a session:

```yaml
decompose:
  - id: where-the-time-went
    whole: lifetime                         # the request above
    parts:
      - {state: on-cpu,   from: cpu.profile,   value: samples}    # count x period
      - {state: runnable, from: sched.runqlat, value: total_ns}
      - {state: blocked,  from: sched.offcpu,  value: total_ns, less: queued_ns}
```

```rust
pub struct Decomposition { pub id: String, pub whole: Whole, pub parts: Vec<Part> }
pub struct Part { pub state: String, pub from: SourceId, pub value: PartValue, pub less: Option<String> }
pub enum PartValue { Samples, Attr(String) }

// and in RunOutput, beside `joins`
pub struct DecompositionOutput {
    pub id: String,
    pub whole_ns: u64,
    pub whole_err_ns: u64,
    pub rows: Vec<Row>,
    /// Signed, and never rounded away: negative is time counted twice,
    /// positive is a state no row covers.
    pub residue_ns: i64,
}
pub struct Row {
    pub state: String,
    pub source: SourceId,
    pub kind: Kind,               // Sampled | Polled | Traced
    pub evidence: Evidence,       // KEY for a row filtered to the task
    pub ns: u64,
    pub resolution_ns: u64,       // a period for a sample count, zero for a trace
    pub short: Option<emit::Shortfall>,
}
```

Refused at validation when a part's source is not in the session or carries a
different `task` than the whole. The residue is the point of the output; a
row that says `interrupted` where a sampled on-cpu part and the kernel's own
figure differ (finding 2) belongs in it once the kernel's figure is a source.

### To whoever owns `src/validate.rs` and `catalog/*.yaml`: refuse a wakeup join onto a drained total — finding 5

`sched.wakeup -> sched.offcpu` credits each drained `sched.offcpu` event — a
hundred blocks — to whichever wakeup came last before the drain, and prints
WAKEUP beside it. What it rests on is proximity in time. Measured: 11 events for
607 blocks, `11 of 11 attributed`, the last hundred blocks credited to a child
that woke the task once.

The invariant is *refused with the reason, never degraded silently*. Wanted, a
backend flag and a refusal:

```yaml
# catalog/sched.yaml, on the sched.offcpu and sched.runqlat backends
drained: true     # one event stands for many occurrences, totalled in the kernel
```

```rust
// in check_joins
Rejection::new(format!(
    "rule `{id}` matches by `wakeup` onto `{to}`, whose events are totals drained \
     from the kernel: each stands for many blocks, and a wakeup can only release one. \
     Join onto a source with one event per occurrence, such as `sched.switches`"
))
```

The alternative — a `weight` attribute the engine counts `matched` in — makes the
count right and the attribution still wrong, which is worse.

### To whoever owns `src/join.rs` and `src/encode.rs`: rank a wakeup join by chain — finding 6

`assemble` ranks attribution *states*, and a wakeup join opens one per wakeup:
1230 states of one or two events each, topped by whichever wakeups preceded a
preemption. The table is headed `heaviest wakeup chains` and never shows the
chain that released the task 602 times. Wanted, for `MatchKind::Wakeup` only,
before the sort and the truncation:

```rust
// merge finished attributions with an equal `key`, summing `matched`,
// keeping the earliest `opened_ns` and the latest `closed_ns`
```

And in `outcome`: `(Outcome::StillOpen, MatchKind::Wakeup)` renders
`never ran`, including for states with events attributed — the task did run.
`still open at the end` is what happened.

### To whoever owns `src/collect/cpu/oncpu.rs`: a closing read from the reap — finding 8

`cpu.oncpu_time` via `fs:proc` on a launched program read 10 to 26 ms short of
the kernel's own figure in every run, always by a multiple of the tick: the cpu
spent after the last poll is in no reading. For a launched program the reap has
the exact answer, `wait4`'s `rusage`, and a closing reading from it would make
the row exact at the end. This is the open question `STATE.md` calls *the
closing read of a polled counter*, met a third time.

### To whoever owns `catalog/cpu.yaml`: what a `cpu-clock` sample counts — finding 2

A sample fires on the time a task holds its cpu, interrupts included; the
kernel's `schedstat` on-cpu figure leaves interrupt time out on a kernel built
with `IRQ_TIME_ACCOUNTING`. The two differ by 0.7 % at 997 Hz and 28 % at
100 kHz, where the interrupts are the profiler's own. Wanted, a caveat on
`cpu.profile`:

> A time-based sample counts the time the task held its cpu, including the
> interrupts that ran on it meanwhile, and at high rates those interrupts are
> this profile's own. The kernel's own on-cpu figure leaves interrupt time out,
> so the two differ by exactly the time the task lost to interrupts: 0.7 % at
> 997 Hz and 28 % at 100 kHz on the machine this was measured on.
## Needs from another stream

### Stream P3 — the I/O path

Six requests from the I/O crossing, `docs/scenarios/cross-io.md`, each with the
measurement that raised it. P3 owns no file under `src/`, so none of these is
landed; the crossing works around none of them in the engine either, and says in
its own session and test where it would have used each.

#### To whoever owns `src/collect/sched/wakeup.rs` and `offcpu.rs` — apply the filter, or refuse it

Defect 1 of `docs/scenarios/cross-io.md`: `--filter "comm == 'x'"` on
`sched.wakeup` and `sched.offcpu` is validated, placed, and never evaluated, and
no note says so — 1528 of 1528 wakeups came back for a `comm` no task has. The
block collectors already show the shape to copy, in
`src/collect/io/block.rs`: a `predicate: Option<FilterExpr>` taken from
`request.filter` at prepare, `crate::filter::matches(predicate, &event)` over the
drained events, and a degradation note with the two counts:

```rust
// in each collector's drain, after the event is built
if let Some(predicate) = &self.predicate {
    if !crate::filter::matches(predicate, &event) { self.filtered_out += 1; continue; }
    self.filtered_in += 1;
}
// and in `degradations()`:
"the filter kept {filtered_in} wakeups and removed {filtered_out}; it ran here, over the \
 drained records, not in the kernel"
```

If that is not wanted, the other honest answer is a refusal at prepare naming the
backend. What is not honest is the current one. Wanted by
`the_scheduler_half_ignores_a_filter_and_says_nothing`, which asserts the defect
and says what to flip.

#### To whoever owns `src/collect/sched/*.rs` — a thread target that compares the thread

Defect 2: `WakeupCollector::new(filter)` and `OffCpuCollector::new(filter, ..)`
receive `Target::first_pid()`, which for `Target::Tid` is the thread id, and the
programs compare it against the task's `tgid`. For any thread that is not its
process's first the run keeps nothing and reports no loss — 0 events through
`--tid`, 66 through `--pid` for a thread that slept 60 times. Wanted, on the
model of `src/collect/io/block.rs`'s `Submitter`:

```rust
enum Whose { Any, Process(u32), Thread(u32) }   // compare tgid, or compare pid (the kernel's tid)
```

`sched.runqlat`, `sched.migrations`, `sched.lock`, `proc.syscall` and `mem.stall`
hand `first_pid()` to a thread-group filter in the same way; that is read from
the code, not measured. Wanted by
`a_thread_target_on_the_scheduler_half_keeps_nothing_and_says_nothing`.

#### To whoever owns `src/join.rs` — refuse `eq` on a scoped key, or rate it lower

Defect 3: `check()` refuses `scoped_eq` on a key without a scope and accepts the
converse, and `evidence()` rates both KEY. On `blk:rq@open_ts`, `eq` compares the
pointer alone and credited 11496 completions to requests that had already
finished, at the same evidence level as the join that credited none. Either:

```rust
// in check():
MatchKind::Eq if is_scoped(rule.via) => Err(unmatchable(
    "`blk:rq@open_ts` names one life of a recycled pointer, and `eq` compares the pointer \
     alone: a completion would land on whichever request last held that address. Use \
     `scoped_eq`",
)),
```

or keep it expressible and rate it `Evidence::Temporal`, since what a bare
recycled pointer establishes is that two events were at one address around the
same time. `examples/cross-io-stale-pointer.yml` uses it on purpose and would
move to a comment showing the refusal.

#### To whoever owns `src/join.rs` and `src/encode.rs` — wakeup attributions grouped by chain

Defect 4: every wakeup opens its own state, so against a stream with one event
per block each attribution has a count of 1 and `top` is key order, not weight.
The table headed *heaviest wakeup chains* then led with `task:0 (idle) ->
task:113142`; every later run that counted the wakeup following each
completion found three kworkers leading and the idle task not among them. Wanted: `assemble()` for `MatchKind::Wakeup` folds
finished attributions by `key` (the rendered chain) before ranking, summing
`matched` — the chain is the thing a reader ranks, not the state.

#### To whoever owns `src/join.rs` — a match that pairs a close with the next wakeup of its task

Boundary 8 is *this completion released this reader*. The engine's `wakeup`
match goes the other way — a wakeup opens, and the woken task's later events are
attributed to it — so against `io.latency` it attributes each request to the
wakeup that released its submitter *before* the request was issued, which for
reads one at a time is the previous read's delivery. The crossing does the pairing
in its test instead. Wanted, one more `MatchKind`:

```yaml
- id: completion-released
  from: io.latency          # opens a state per task named on the event
  to: sched.wakeup          # the first wakeup of that task after it closes it
  via: task
  match: woke               # evidence WAKEUP; unmatched = the task never slept
  woken: wakee_tid          # the attribute of a `to` event naming the woken task
```

with `unmatched` meaning a completion no wakeup followed — a reader that never
went to sleep, which is itself a finding and should be counted as one.

#### To whoever owns `src/request.rs`, `src/join.rs` and `src/encode.rs` — a decomposition

The table in `docs/scenarios/cross-io.md` is computed by the test from raw
events, because the engine can join two streams and cannot say *these segments
of this whole, with this residue*. P2 needs the same thing for on-cpu, runnable
and blocked. What the I/O crossing would declare:

```yaml
decompose:
  id: one-read
  whole: io.syscall                   # the enclosing span; absent in this build,
                                      # so the table says `NOT SEEN` on its line
  by: task
  segments:
    - {from: io.queue,   join: halves,     label: software queue}
    - {from: io.latency, join: halves,     label: device}
    - {join: completion-released,          label: completion -> wakeup}
  residue: required                   # printed, never rounded into a segment
```

and what it would print is the table in that document: one line per segment with
the evidence of the boundary that produced it, the weakest link that attaches it
to the whole, and a residue line. Until a whole exists in this build (`fentry`),
the useful half is the per-segment evidence and the residue against a whole the
session is told about, such as a launched program's own timing.

Two smaller ones, noted rather than asked for: **one target per session** is why
the crossing runs two sessions over one window (`io.queue` has only `system`,
and defect 2 rules out the thread), and **`sched.wakeup` carries the wakee's
`comm` and not the waker's**, so a chain names its kworker only by a `/proc`
read after the fact, which reports the work the worker took up since.

---
## Needs from another stream — stream P4 (crossed families, the machine as a confounder)

Five requests from the seventh wave's machine-confounder crossing. None of them
blocked the stream: each is worked around in `tests/cross_machine.rs` and
explained in `docs/scenarios/cross-machine.md`. Each one is also a place where
the workaround is weaker than the tool should be.

### To whoever owns `src/join.rs` and `src/catalog.rs`: a match kind whose evidence is `TEMPORAL`

`Evidence::Temporal` exists and nothing produces it: `join::evidence` maps
`wakeup` to `WAKEUP` and every other match to `KEY` or `IDENTITY`. A confounder
check is two things happening on the same core, or on the same machine, at the
same time, and writing it as `match: eq` on `cpu` would print `KEY` beside it.
That is the dressing-up the evidence ladder exists to prevent. So the
frequency crossing is arithmetic in a test rather than a `joins:` entry.

```rust
// src/catalog.rs
pub enum MatchKind {
    Eq,
    ScopedEq,
    Interval,
    Wakeup,
    /// The downstream event's window overlaps the upstream event's window on
    /// the same key (or on no key). Evidence is always TEMPORAL.
    Overlap,
}

// src/join.rs, in `evidence`
MatchKind::Overlap => Evidence::Temporal,
```

The semantics are these. A `from` event with an `interval_ns` attribute (every
polled source carries one) opens a state covering `[ts - interval_ns, ts]`,
filed under its `via` key, and it replaces the previous state for that key. A
`to` event is attributed to every open state its own window overlaps. The
attribution carries the overlap as a share of the `to` event's window, so a
count spanning ten frequency ticks is split ten ways rather than credited to
the last tick. `via` should be allowed to be absent for machine-wide state
(`mem.pressure` has no key).

### To whoever owns `src/collect/mod.rs`: more than one target in one run

`CollectError::MixedTargets` refuses any session whose requests name two
targets. Every confounder check has exactly that shape: the machine's state is
`system` or `cpu`, and the measurement is `pid`, `launch` or `cgroup`. The
frequency crossing has to run a second `omn` beside the first, counting the
loop's own instructions by pid, because a `cpu`-target counter also counts
whatever else ran on that core. Measured under load, that was 70% of the core.
The reclaim crossing reads the machine's PSI instead of the cgroup's, and the
machine's value dilutes the cgroup's stall by the number of busy cpus (12.5 ms
read as 6.0 ms).

The minimum that would serve: allow any number of targets whose kinds are among
`system`, `cpu` and `iface` (the machine's) beside at most one workload target
(`launch`, `pid`, `tid`, `cgroup` or `container`). Only the workload target
drives the stop condition and the launch.

```rust
// src/collect/mod.rs: what `MixedTargets` would still refuse
fn compatible(a: &Target, b: &Target) -> bool {
    a == b || a.is_machine() || b.is_machine()
}

// src/request.rs
impl Target {
    /// A target that names part of the machine rather than a workload: its
    /// collectors do not act on a program and do not decide when a run ends.
    pub fn is_machine(&self) -> bool {
        matches!(self, Target::System | Target::Cpu(_) | Target::Iface(_))
    }
}
```

### To whoever owns `src/collect/cpu/` and `catalog/cpu.yaml`: a per-tick read of a counting PMU event

`cpu.instructions` and `cpu.cycles` offer `count`, which is one number for the
window, and `sample`, which is refused until sampling lands with stacks. So no
verdict can say *when*. `BRIEF.md` asks for "throttled for 1.2 s of 5 s, and the
instruction rate over that interval is not comparable to the rest", and the
tool has a frequency timeline and no instruction timeline to split it with.
The counter is already open and already read at the end, so what is wanted is
the same read at an interval:

```yaml
# catalog/cpu.yaml, on every pmu counting entry
modes: [count, poll, sample]
emits: [summary, rate, timeline, raw, hist, topk, stack]
```

Here `mode: poll@100ms` reads the counter each tick and emits the delta, with
`interval_ns` like every polled source, so the `Overlap` match above can pair
it with `power.freq` tick by tick.

### To whoever owns `catalog/power.yaml`: `power.throttle`'s eBPF backend names a temperature tracepoint

The first backend of `power.throttle` is `bpf:raw_tp` requiring
`raw_tracepoint:thermal_temperature`. That tracepoint reports a thermal zone's
temperature on each update, not a throttling event. A collector written against
it would measure temperature and call it throttling. The events that mean a
throttle are `thermal_zone_trip` (a trip point crossed) and `cdev_update` (a
cooling device changing state), and on AMD neither the hardware nor the kernel
publishes the package power limits' effect as an event at all. The effective
frequency is the only observable of it. This matters now because the backend is
reported `available` here, so the first collector written for it will be taken
at its word.

### To whoever owns `src/collect/io/writeback.rs`: 2.6 MiB of flusher progress beside 90 MiB of kworker writes

In one run of `examples/cross-machine-reclaim-io.yml` with the program capped
at 32 MiB on btrfs, `io.writeback` reported 667 pages of progress while
`io.latency` attributed 90 MiB of writes on the same device to kworkers during
the program's run. The collector already documents that `nr_pages` is a budget
spent rather than a total. Whether memcg-forced writeback on btrfs simply does
not go through `wb_writeback`, or the budget arithmetic undercounts, is not
settled. It was not investigated further, and `io.writeback` was kept out of
the session so that nothing depends on it.

---

## 42. The queue wait inside a block, and the double count it was — closed

The seventh wave's compute crossing filed it as its finding 1 and could not fix
it: `sched.offcpu` times a block from the switch that took the task off the cpu
to the switch that gave it one back, so the run queue wait after the wakeup is
inside `total_ns` — and inside `sched.runqlat` as well, which opens a wait at
that wakeup and counts it too. A decomposition adding the two sources counted
those nanoseconds twice, and nothing anywhere said how many they were.

**What it needed was the wakeup between the two switches**, which the program
did not see. It has a second program now, on `sched_wakeup` — deliberately that
tracepoint and not `sched_waking`, because `sched.runqlat` measures from
`sched_wakeup` and a wait measured from the other one would be a different
number that looked comparable. It stamps the moment into the in-flight entry the
`sched_switch` program created, and never inserts: a wakeup for a task this run
did not see block is not this source's business, and inserting there would grow
the map with every wakeup on the machine.

Every row now carries `queued_ns` and `blocked_ns` beside `total_ns`, which is
left alone so that nothing written against it changes meaning.

**The ground truth is the kernel's own `run_delay`.** In the crossing's own
session — 100 sleeps of 5 ms in `SCHED_BATCH` beside a spinning shell on one cpu
— the source reported **93.6 ms** of queue wait against the kernel's **96.5 ms**
for the same loop: under 30 microseconds per wakeup apart, on an independent
counter rather than a rearrangement of the tool's own numbers. The crossing's
test asserts that agreement now, where it used to derive the wait by
subtracting the requested sleep from the block.

**A zero is not a measurement here, and the run says which zero it is.** A block
whose releasing wakeup the run did not see — the task was already asleep when
the programs attached, or was woken by a path that does not raise
`sched_wakeup` — reports `queued_ns` of zero because it is unknown, and the
collector counts those blocks and says so in its notes. A decomposition
subtracting an unknown zero would be subtracting a guess.

Two things this does not close. A task woken several times before it runs keeps
the last stamp, which is the wait closest to the switch-in and is what
`sched.runqlat` does with the same tracepoint, so the two agree — but both
under-report a task that was woken, preempted before running, and woken again.
And `sched.offcpu` still credits a block in full to the interval in which it
ended, which is unchanged and is a different caveat.
## Needs from another stream — the eighth wave, stream R (the run model)

Written by the stream that let a workload and machine targets share one run,
stamped the launched program's lifetime, and made the heap sources share one
shim pipe per launch. `docs/families/runner.md` has the measurements. In the
order they are worth doing.

### To whoever owns `src/emit.rs` (nobody), and stream J for `src/encode.rs`: publish the lifetime

`Collection::lifetime: Option<collect::Lifetime>` is set on every collection of a
run that launched a program, and nothing copies it into `emit::Results`, so no
format carries it. It is the `whole` the decomposition needs (P2's finding 4).
Wanted:

```rust
// src/emit.rs, in `Results`
/// When the run's launched program ran, from the gate to its exit. Absent
/// when nothing was launched.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub lifetime: Option<crate::collect::Lifetime>,

// src/emit.rs, where `Results` is built from a `Collection`
lifetime: collection.lifetime,
```

and in the text encoder, once per run rather than per result, something like
`the launched program ran 212.4 ms (gate to exit; the exit is bracketed to the
last 209.5 ms)` -- or `..., stopped by the run` when `terminated`. The bracket
has to be printed beside the length: with the pidfd, `exited_ns` is typically
2 ms late while `exited_err_ns` is the whole time the runner slept, and a reader
given only the first would over-trust a residue.

### To stream J for `src/request.rs`: `Target::is_machine`

The run model needs it and it is a fact about targets, not about the runner. It
is a private `is_machine` in `src/collect/mod.rs` today; once this lands that
function goes:

```rust
impl Target {
    /// A target that names part of the machine rather than a workload: its
    /// collectors do not act on a program and do not decide when a run ends.
    pub fn is_machine(&self) -> bool {
        matches!(self, Target::System | Target::Cpu(_) | Target::Iface(_))
    }
}
```

### To the coordinator, for `tests/cross_memory.rs` and `docs/scenarios/cross-memory.md`: D3 is fixed, and its tripwire is red

`d3_two_shim_sources_on_one_launch_are_refused_as_a_request_rather_than_as_the_build`
fails now, as its own message says it should. `mem.heap.alloc` and
`mem.heap.retained` on one launch run, exit 0, from one pipe. Replace the three
assertions after `let output = run_written(&[], SESSION);` with:

```rust
assert!(
    output.status.success(),
    "two shim sources on one launch share one pipe: {}",
    stderr(&output)
);
```

and rename it `d3_two_shim_sources_on_one_launch_share_one_pipe`. D3 moves to
fixed in `docs/scenarios/cross-memory.md`, and the crossing's two sessions can
fold into one. `tests/runner.rs`'s
`two_heap_sources_on_one_launch_share_the_shim_and_each_sees_all_of_it` is the
ground truth for the sharing itself.

### To the coordinator and the owners of the files named: "one run observes one target" is no longer true

Comments that explain a design by that rule, and now mislead:

- `src/main.rs:737` and `src/encode.rs:1518` -- "one window against one target";
  one window is still true, one target is not. Joins now merge events of a
  workload and of machine targets, which is the point.
- `tests/cross_io.rs:686`, `tests/scenario_mem.rs:2037`,
  `tests/scenario_sched.rs:2084` and `examples/cross-machine-frequency.yml` --
  each explains a split or a system-wide target by it. The frequency crossing's
  second `omn` can become one session: `cpu.instructions` on the launch beside
  `power.freq` and `cpu.cycles` on the core.
- `DECISIONS.md`'s session-versus-daemon line is untouched: a run is still one
  window and one document, and `docs/families/runner.md` says which two
  extensions would not be.

### To whoever owns `tests/scenario_cpu.rs`: the known-instruction scenario flaps under load, before this wave too

`a_known_number_of_retired_instructions_is_counted_as_that_number` failed 4 of
25 on the parent of stream R's commit and 4 of 25 on it, interleaved, with half
the cores spinning; it failed once in three runs on an idle machine. The failing
runs overshoot by +0.69 %, and the large run's excess over 10^9 (13.85 M) is
twice the startup run's count (6.97 M), which points at something counted twice
in the large run rather than at noise. Not investigated further.
## Needs from another stream — the eighth wave, stream U (USDT and the `runtime` family)

What the USDT work needed from files it does not own. The record of what was
measured is `docs/families/runtime.md`.

### To stream T, for `src/bpf/attach.rs`: the `uprobe` PMU needs CAP_SYS_ADMIN, so add a uprobe-multi attach

`attach::uprobe` had never been called. Called for the first time, with
`cap_bpf,cap_perfmon+ep` effective (`CapEff: 000000c000000000`) and
`perf_event_paranoid = -1` on 7.2.6:

```
perf_event_open, uprobe PMU, pid = target    EACCES
perf_event_open, uprobe PMU, pid = 0         EACCES
perf_event_open, uprobe PMU, cpu = 0         EACCES
perf_event_open, kprobe PMU, cpu = 0         fd
BPF_LINK_CREATE, BPF_TRACE_UPROBE_MULTI      fd
```

`perf_uprobe_event_init` keeps `CAP_SYS_ADMIN` where the kprobe PMU accepts
`CAP_PERFMON`. So `bpf:uprobe` cannot attach under the project's privilege
model, and a uprobe-multi link (6.6) can -- with a process scope, and with
semaphores maintained by the kernel through `ref_ctr_offsets`. Stream U carries
the link in a fenced section of `src/bpf/usdt.rs` and would like it here:

```rust
/// A uprobe-multi link: one loaded program on `offsets` in `path`, for `pid`'s
/// thread group or for every process when `None`.
pub struct UprobeLink { /* link fd, program fd */ }

pub fn uprobe_multi(
    path: &Path,
    offsets: &[u64],
    ref_ctr_offsets: Option<&[u64]>,
    cookies: Option<&[u64]>,
    pid: Option<i32>,
    instructions: &[Insn],
    prog_name: &str,
) -> Result<UprobeLink, String>;
```

And the probe half, which is finding 15's shape: `probe_type_support("uprobe")`
answers from `/sys/bus/event_source/devices/uprobe/type`, so `omn list` reports
`uprobe` **available** on this machine while every open is refused. It should
be answered by what the attach actually needs.

### To whoever owns `src/bpf/syscall.rs`: `prog_load` with an attach type, and `link_create`

A uprobe-multi program must be loaded with `expected_attach_type =
BPF_TRACE_UPROBE_MULTI` (48), which `prog_load` never sets; fentry needs the
same field plus `attach_btf_id`, so stream T will want this too and it should
land once:

```rust
pub fn prog_load_for(
    prog_type: u32,
    expected_attach_type: u32,
    attach_btf_id: u32,
    instructions: &[Insn],
    name: &str,
    license: &str,
) -> Result<OwnedFd, LoadError>;

pub fn link_create_uprobe_multi(
    prog_fd: RawFd,
    path: &CStr,
    offsets: &[u64],
    ref_ctr_offsets: Option<&[u64]>,
    cookies: Option<&[u64]>,
    pid: u32,
) -> io::Result<OwnedFd>;
```

### To whoever owns `src/bpf/insn.rs`: five encodings and a helper

Declared locally in `usdt.rs`'s `mod local` for now: `Insn::lsh64_imm`,
`Insn::arsh64_imm`, `Insn::or64_imm`, `Insn::add64_reg`,
`Insn::ld_imm64(dst, i64) -> [Insn; 2]`, and
`helper::PROBE_READ_USER = 112`. Sign-extending a USDT argument from its
declared width is `lsh` then `arsh`; nothing else here needed a 64-bit constant
until a descriptor carried one.

### To the coordinator: one line in `src/bpf/mod.rs`, and the indexes

`pub mod usdt;` was added to `src/bpf/mod.rs`, which no stream owns and which a
new file under `src/bpf/` cannot do without. Seven sessions were added,
`examples/runtime-usdt-{probes,libc,gc,alloc,threads,flood,absent}.yml`, and
`examples/runtime-not-built.yml` was rewritten; `examples/README.md` and
`GUIDE.md` need their rows, which is the one red test on `feat/usdt`
(`cli::the_session_indexes_hold_exactly_the_sessions_on_disk`).
`docs/scenarios/runtime.md` says USDT cannot be verified here; it now is.

### To whoever owns `src/resolve.rs`: the kernel placement is true for part of a filter

`placement` credits every `bpf` backend with "evaluates predicates in the
program". For USDT, the part of a filter about the probe (`provider`, `probe`,
`object`) is evaluated before anything is attached and a probe it excludes
costs nothing; a predicate on `comm` is evaluated per hit in the collector,
after the trap has been paid. The collector reports how many hits it removed;
the resolved record still says all of it ran in the kernel.

### To `sched`, for `sched.mutex`: glibc publishes the mutex

`libc:mutex_entry`, `mutex_acquired` and `mutex_release` are in this machine's
`libc.so.6`, each with the mutex address as its first argument -- in `rdi`,
`rbx` or `r8` depending on the site, which is why `usdt.rs` builds one program
per site. `sched.mutex` waits on "the
preload shim, pointed elsewhere"; a USDT route needs no preload and reaches a
running process. The lock wait is `acquired - entry` per thread per address.

### Two tests that fail under a parallel workspace run and pass alone

Neither is in a file stream U touched. Each failed once in `cargo test
--workspace --no-fail-fast` and passed three times out of three on its own:

- `collect::tests::what_a_run_reports_spending_is_the_cpu_it_actually_burned`
- `cross_compute::a_half_blocked_task_decomposes_into_what_the_kernel_says_it_did`

---

## 43. The lifetime reaches the document, and the two halves it was split across

The run model stream stamped a launched program's lifetime onto every
`Collection` and filed the other half as a request, because `src/emit.rs` has no
owner in the eighth wave and `src/encode.rs` is stream J's. The coordinator
landed the first half: `Results` carries `lifetime`, so the four structured
formats carry it with no encoder change at all, and
`the_lifetime_reaches_the_results_document_with_its_bracket` holds them to it.

```json
"lifetime": {"released_ns": 327678333759, "exited_ns": 327992275000, "exited_err_ns": 13361200}
```

**The bracket travels with the length, and that is the part worth keeping.**
`exited_ns` is when the wait found the program gone and `exited_err_ns` is how
long before that it was last seen alive, so the exit happened somewhere in
between and nothing here can say where. A reader given only the length would
over-trust a residue smaller than the bracket — 313.9 ms of life above, bracketed
to 13.4 ms — which is exactly the arithmetic a decomposition is about to do with
it.

**What is left is one line in the text encoder**, which belongs to whoever owns
`src/encode.rs`: the four structured formats are the ones a decomposition and a
model read, and the text one is the one a person reads. Until it lands, a person
running with the default format cannot see the whole that the residue is a
residue of.

### The tally, for the eighth wave, counted rather than rounded

Four `make check` runs here while two sibling streams ran their own suites, at a
load average between 9 and 11 on 24 cores. Four failures, **each a different
test**, and every one of them passed in isolation immediately afterwards:

- `collect::mem::wss::tests::a_process_that_holds_much_and_touches_little_reports_the_little`
- `tests/scenario_mem.rs::compaction_is_seen_where_the_kernel_says_it_happened_and_skipped_loudly_where_it_did_not`
  — this one was **not** the machine and is fixed: it treated a kcompactd wake
  as proof that compaction ran. The others are.
- `tests/scenario_gpu.rs::a_resident_kernel_separates_from_an_idle_device_on_occupancy_and_on_power`,
  with `nvidia-smi` reporting the device at 100 % utilisation and 178 MiB held
  by somebody else at the moment it ran.
- `tests/scenario_mem.rs::both_huge_page_backends_read_the_same_promotion_counter`

Plus the `io` pair `docs/scenarios/io.md` already names, which failed again.

The ninth wave added one while streams V and P were both driving the disk:
`tests/cross_io.rs::the_crossing_sessions_run_and_say_what_they_found`, which
passed alone at once. `both_huge_page_backends_read_the_same_promotion_counter`
failed again on the same merge, its second time on this list.

The rule this suggests, and it is not a new one: **a red suite on this machine
is checked against this list before it is believed**. What is worth watching is
whether the same test starts failing repeatedly, which is a defect, rather than
a different one each time, which is the machine.

---

## 44. A bucketed source stamps the window's first episode, not the drain

The seventh wave's machine crossing filed this and worked around it: a source
that builds its histogram in the kernel emits one event per row per window, and
that event was timestamped at the drain that read it — the window's *end*. A
join that **opens** on such a source therefore opened at the close, and
everything that happened in the same window as the first episode fell outside
the state it was meant to open. The crossing's workload sleeps 300 ms between
writing and reading for that reason, and its `KEY` assertion counts only the
reads issued after reading back began.

`mem.reclaim`'s kernel-side row now records when the window's first episode
*began* — recovered as `now - duration` at the moment the row is created, which
is the first completed episode of that key in that window, since the rows map is
drained and emptied every window — and the event is stamped there.

**`ts_err_ns` stays zero, and that is the part worth arguing.** The field means
*how far off `ts_ns` may be*, and it is non-zero when a timestamp was converted
from a device's own clock. The instant here is exact and taken in this domain;
what is wide is the window the row aggregates, and `window_ns` beside the value
already says how wide. Carrying the span as a clock error would have overloaded
one field with two meanings, which is the conflation this project refuses
everywhere else.

**`sched.runqlat` has it too now**, for the reason that made it next: it is an
input to thread state analysis, and a decomposition that opens a state on it
inherits exactly this. Measured on a one-second machine-wide run: **586 events
carrying 584 distinct instants**, where stamping at the drain gives about ten
however busy the machine is. `a_bucketed_source_does_not_stamp_every_row_of_a_window_with_one_instant`
asserts that shape rather than a threshold.

Its own layout test caught two mistakes in the five minutes it took: the new
field's offset collided with `comm`, and the value region then overlapped the
key on the program's stack. Both would have produced plausible numbers.

**And then all six of the rest**: `sched.lock`, `sched.futex`, `sched.irq`,
`net.qdisc`, `net.softirq` and `proc.syscall`. Every bucketed source in the
build now stamps where its window began. Measured over one second each:

| Source | Events | Distinct instants |
|---|---|---|
| `sched.runqlat` | 586 | 584 |
| `proc.syscall` | 2202 | 2202 |
| `sched.lock` | 174 | 174 |
| `sched.irq` | 53 | 53 |
| `sched.futex` | 97 | 97 |
| `net.softirq` | 33 | 33 |

Stamping at the drain gives about ten, however busy the machine is. `net.qdisc`
produced no rows on an idle loopback and is the one of the seven not measured
here; its path is the same four instructions.

The mechanism is four instructions and one field per row -- `now - duration`
where the row is created -- and what made it worth doing once per source rather
than in a shared layer is that each keeps its own value layout: three of the six
needed their value region moved down the program's stack to make room, and their
own layout tests said so before the kernel did.
## Needs from another stream — stream T (fentry, and the five sources behind it)

What the eighth wave's fentry stream needed from files it did not own, and
three losses it found in a shape other programs share. `docs/families/io.md`
and `docs/families/net.md` are the stream's record; this is only what someone
else has to do.

### To whoever owns `src/bpf/btf.rs`: a function's type id and prototype

`src/bpf/fentry.rs` carries a small BTF index of its own, because an fentry load
needs the type id of a `FUNC` and `Btf` keeps its ids private. It reads the same
file, so the two cannot disagree, but it is a second parser. Wanted:

```rust
pub struct Function { pub id: u32, pub params: Vec<String>, pub returns: String }
impl Btf {
    /// The `FUNC` of this name and the prototype it points at.
    pub fn function(&self, name: &str) -> Result<Function, String>;
}
```

With it, `fentry::FunctionIndex` goes and `fentry::expect` calls `Btf`.

### To whoever owns `src/bpf/syscall.rs`: four commands `fentry.rs` issues itself

- `prog_load` cannot name an attach type or a target: a tracing load needs
  `expected_attach_type`, `attach_btf_id` and `attach_btf_obj_fd`, which sit
  past the end of `ProgLoadAttr` (offsets 68, 108 and 112 of `union bpf_attr`).
  Wanted: `pub fn prog_load_tracing(attach_type: u32, attach_btf_id: u32,
  instructions: &[Insn], name: &str, license: &str) -> Result<OwnedFd, LoadError>`.
- `raw_tracepoint_open` takes a `&str`, and a tracing program is attached with
  no name at all. Wanted: `name: Option<&str>`.
- `map_update` exists under `#[cfg(test)]` only; the histogram's generation word
  is written from userspace on every drain. Wanted: the same function, public.
- `BPF_OBJ_GET_INFO_BY_FD`, for `bpf_prog_info.recursion_misses`. Wanted:
  `pub fn prog_info(fd: RawFd, bytes: usize) -> io::Result<Vec<u8>>`.

`fentry.rs` issues all four itself until then, each with its own attribute
struct and a layout test.

### To whoever owns `src/bpf/mod.rs`: five module lines

`fentry`, `io_fsync`, `io_vfs`, `net_connect` and `net_socket` are declared in
`src/bpf/attach.rs` with `#[path]`, because `mod.rs` belonged to no stream of
the wave. They belong beside the other programs:

```rust
pub mod fentry; mod io_fsync; mod io_vfs; mod net_connect; mod net_socket;
```

with the types re-exported the way `io::BlockProgram` is, and the five
`#[path]` lines removed from `attach.rs`. Nothing else changes.

### To whoever owns `src/bpf/insn.rs`: one width, three instructions, two helpers

A typed load has to be exactly the member's width, so `BPF_H` is needed; and
`add64_reg`, `lsh64_imm` and `jge_reg` are hand-encoded in `fentry.rs`. The
helpers `GET_SOCKET_COOKIE` (46) and `GET_CURRENT_TASK` (35) are declared
locally by `fentry.rs` and `io_fsync.rs`.

### To whoever owns `src/bpf/runqlat.rs` and `src/bpf/proc_syscall.rs`: three losses nothing counts

Measured while writing `io.syscall` and `io.fs`, which have the same shape, and
now counted there; each applies to these two as they stand.

1. **A drain racing the program's increment.** Both drain their row map with
   lookup-and-delete while the programs write into it. A program that has looked
   a row up adds to it a few instructions later; a drain that took the row in
   between has copied it already, and the increment lands in a freed element --
   or, the map being preallocated, in whatever key the element went to next.
   Measured: 55192 counted and 44807 dropped for 100000 reads, one gone without
   a trace. `fentry::HistogramProgram` keeps two maps and a generation word and
   drains only the one out of use for a whole interval; the same would serve.
2. **The kernel skipping a run of a program already running on that cpu.**
   Raw tracepoints and trampolines both guard each program with a per-cpu
   counter, and a run that finds it taken is skipped whole, counted only in
   `bpf_prog_info.recursion_misses`. For a trampoline that is preemption; for a
   raw tracepoint, whose probes run with preemption off, it is an interrupt
   re-entering the program on its own cpu. **Observed on the raw-tracepoint
   side**: `io.size`'s block programs skipped 766 and 1237 runs in one run
   each of `tests/scenario_io.rs`, and a thread's 4096 direct reads were short
   by 2 to 23 requests in six runs out of six, with nothing counted. Not
   observed on the trampoline side. `fentry::skipped(programs)` reads it for
   any set of program descriptors; see the next entry.
3. **(fentry only) the kernel skipping the exit half of a call in flight** when
   any tracer attaches to or detaches from the same function. Not these two's
   problem -- a raw tracepoint has no trampoline -- and written here so that
   nobody reaches for fexit expecting otherwise.

### To whoever owns a raw-tracepoint program: the kernel's skips are a loss nothing counts

Every raw-tracepoint program runs behind a per-cpu guard, and a request, packet
or switch that arrives while the program is still running on that cpu -- from
an interrupt, since probes run with preemption off -- is not seen by it at all.
Its drop counters and its ring cannot say so; the kernel counts it in
`bpf_prog_info.recursion_misses` and nowhere else. Measured on the block layer
(the entry above): hundreds per run under a loaded device, and a scenario that
had passed with a one-percent tolerance for waves was hiding it.

Stream T now reads it for every raw-tracepoint program it owns -- the block and
writeback programs and the five `net` ones -- and counts it in
`Losses::dropped` with a reason. The files outside this stream that load a
`BPF_PROG_TYPE_RAW_TRACEPOINT` program and do not read it: `runqlat.rs`,
`offcpu.rs`, `wakeup.rs`, `migrate.rs`, the `sched_*.rs` programs,
`proc_syscall.rs`, `proc_events.rs`, `mem_alloc.rs`, `mem_program.rs` and
`mem_stall.rs`. Wanted in each: keep the program descriptor rather than `_`, and

```rust
pub fn skipped(&self) -> u64 {
    crate::bpf::attach::fentry::skipped([&self.program /* , every program */])
}
```

read by the collector beside its drop counters. The count is the machine's, not
the target's -- the program runs on every event and filters after -- so it is an
upper bound on the target's share, and the reason should say that. Once
`fentry.rs` moves under `bpf/mod.rs` (above) the path is `crate::bpf::fentry`.

### To whoever owns `src/join.rs`: an event without the rule's key is unattributable, not a malformed rule

`missing_key` refuses the whole rule at the first `to` event that lacks the key.
`net.tcp.lifetime` legitimately emits such events -- a server-side socket whose
cookie nothing ever asked for -- so `net.tcp.connect -> net.tcp.lifetime` on
`sock` was refused over ten connections that all joined.
`examples/net-handshake-to-close.yml` filters on `cookie > 0` to get past it.
Wanted: such an event counted as unattributed, with a count of its own in the
join's report, and the refusal kept for a rule whose side can *never* carry the
key -- which `check` already knows from the catalog.

### To whoever owns `tests/family_sched.rs`: a fixed attach delay

`a_task_moved_between_two_cpus_is_counted_once_per_move` gives `omn` 800 ms to
attach before it starts moving the task, and failed once in this wave's suite
with 0 of 20 counted; it passes alone. fentry attaches rebuild trampolines,
which the kernel serialises machine-wide: a 1 ms run of `io.syscall` takes
660 ms, and six at once 2.2 to 2.4 s each. Wanted: wait for the run to report
that it is attached -- or keep moving the task until the run ends -- rather than
for a fixed delay.

### To whoever owns `src/request.rs`: `count` on a kernel-built histogram

A counted `io.syscall`, `io.fs` or `net.socket` is still a histogram -- rows of
`(key, bucket, count)` summed over the run -- and the mode x emit matrix refuses
`hist` and `topk` in `count` because a count is one number per series. For these
three it is not. Not blocking: `trace` serves both.

### To the coordinator

- Rows for `examples/io-three-layers.yml`, `examples/io-syncs.yml`,
  `examples/net-fentry-sweep.yml` and `examples/net-handshake-to-close.yml` in
  `examples/README.md` and `GUIDE.md`; the index test is red on this branch
  until then.
- `docs/bpf-layer.md` lists fentry under "What is not here yet". It is here:
  `src/bpf/fentry.rs`, attached through `BPF_RAW_TRACEPOINT_OPEN` with no name,
  and the requirement class is `fentry:<function>`, answered from BTF and a
  load. `CATALOG.md` may want the class named too.
- `STATE.md`'s table of what waits for fentry: `io.syscall`, `io.fs`,
  `io.fsync`, `net.tcp.connect` and `net.socket` collect through it.

---

## 45. The mark reaches `rate`, `topk` and `stack`

Finding 37 scoped itself to `hist`, `summary` and percentiles because three
documents had agreed that scope, and stream F wrote the argument for widening it
on its way out rather than widening it unilaterally. This is that widening,
taken by the coordinator because `src/emit.rs` has no owner in the eighth wave.

A rate, a ranking's totals and a profile's weights are understated by exactly
the records the ring never delivered, and understated in the one direction:
what a ring drops is what arrived at the busiest moment. So all three carry
`Shortfall` now, in the four structured formats, with a clean run gaining
nothing — the negative matters as much as the positive, or the mark becomes
noise and readers learn to ignore it.

**Where each sits, and why it is not folded into what was already there.**
Stream F's open question was how the mark should read next to `Rest` and
`unattributed`, and the answer is that neither is the same fact:

- **`Rest`** is what a `topk` left out *and measured* — the four hundred and
  ninety devices below the top ten, whose total is known. The mark is what
  nothing measured at all. A reader choosing what to look at next is making the
  same decision a percentile is read for, and a ranking whose totals are lower
  bounds can be a ranking in the wrong order.
- **`unattributed`** counts events that arrived carrying no stack identity. The
  mark counts records that never arrived. One says the profile is missing its
  labels and the other says it is missing its samples, and a reader given one
  number for both could not tell which profile they were holding.

`stack` carries it once per table rather than once per stack, because a ring
drops without regard to which stack the record would have joined — the same
reason the distributions carry the run-wide fraction on every series.

**What is left is the text encoder**, which belongs to whoever owns
`src/encode.rs`: the structured formats are what a decomposition and a model
read, and the text one is what a person reads. Two oversteps are declared rather
than hidden — adding a field to an enum variant breaks every destructuring of
it, so four patterns in `src/encode.rs` gained a `..` and two fixtures gained a
`short: None`.

---

## 46. What the eighth wave's merge exposed: the file without the test is where it landed

The fentry stream owns `src/bpf/net*.rs`; the coordinator added a field to every
bucketed row while it worked, `net_softirq.rs` included. The merge combined both
and the result was:

```
a network source must measure the network vectors and no others: {"vector 1760": 30}
```

A softirq vector only goes up to nine. The row value had grown from sixteen
bytes to twenty-four and the region was never moved, so it ran from `-48` into
the row key at `-32` — the program wrote its timestamp over the vector number,
and the drain read back a vector that was really half a nanosecond count.

**It is not the merge's fault and it is not fentry's.** The same change was made
to eight programs. Five of them have a stack-region test and a byte-coverage
test, and those five *refused it at once*: `row value overlaps row key`, by name,
in `sched_lock`, `sched_futex`, `sched_irq` and `runqlat`, twice in the space of
five minutes. Three of them — `net_softirq`, `net_qdisc` and `proc_syscall` —
had neither test, and that is where the defect survived to be found by another
stream's scenario two merges later.

`net_qdisc` and `proc_syscall` turned out to be clean, checked by hand and then
by the tests they now have. All three have both tests as of this finding, so the
set of programs keeping a row and the set with a layout test are the same set.

**The lesson is not "write more tests".** It is that this particular test pays
for itself on a schedule nobody chooses: it costs twenty lines once, and it
catches a class of defect whose symptom is *a plausible number in the wrong
field* — which is the one failure this project treats as worse than a crash,
because nothing downstream can tell it from a measurement.
## Needs from another stream — the eighth wave, stream J (the join engine)

Six requests, none blocking: everything the brief gave stream J works with its
scenarios, and `docs/families/join.md` is the record. Each is a place where
what the engine can now say is limited by something outside it.

### To the coordinator, for `examples/README.md` and `GUIDE.md`: three rows

`the_session_indexes_hold_exactly_the_sessions_on_disk` is red on
`feat/join-engine` for exactly these three sessions, as the brief anticipates.
The same three rows go in both files:

```
| `join-between-residency.yml` | What moved the resident size, asked by key and by the clock -- and a decoy the clock gets wrong, at TEMPORAL. |
| `join-decompose-memory.yml` | The rise in resident size split into live and retained bytes, with a signed residue the program predicts. |
| `join-decompose-compute.yml` | One task's wall clock split into on-cpu, runnable and blocked, each row's kind and resolution, and its releasers ranked by chain. |
```

The scenario documents also still describe as open what this stream closed:
`docs/scenarios/cross-memory.md` D1, `cross-compute.md` findings 5 and 6 (and
the `never ran` wording), `cross-io.md` defects 3 and 4.

### To whoever owns `CATALOG.md`: the `drained` backend field

`Backend::drained` is new in `src/catalog.rs`, beside `equivalent`:

```yaml
- {id: "bpf:raw_tp", requires: [...], cost: med, drained: true}
```

It means one event stands for many occurrences, added up in the kernel and
drained on a tick, and it is what lets the validator refuse a `wakeup` join onto
such a source before the run. §1 of `CATALOG.md` should list it with that
sentence.

### To whoever owns each family's catalog: `drained: true` on the rest

Only `sched.offcpu` and `sched.runqlat` carry it, the two the compute crossing
measured. Every collector below marks its rows with `weight` or `window_ns`, so
a `wakeup` join onto any of them is refused anyway, but after the run, on the
first event, which costs the measurement. Flagging them moves the refusal
before it. Read from the code, not measured:

| Source | Backend served | Marks its rows with |
|---|---|---|
| `sched.migrations`, `sched.lock`, `sched.futex`, `sched.irq`, `sched.numa_balance` | `bpf:raw_tp` | `window_ns`, and `weight` for lock, futex, irq |
| `proc.syscall` | `bpf:raw_tp` | `weight`, `window_ns` |
| `net.softirq`, `net.qdisc` | `bpf:raw_tp` | `weight`, `window_ns` |
| `gpu.job` | `bpf:tp` | `window_ns` |
| `mem.stall` | its eBPF backend | `weight`, `window_ns` |
| `mem.heap.alloc` | the backend `src/collect/mem/allocations.rs` serves | `weight`, `window_ns`. Only that backend: the shim streams one event per call, so `always_drained` stays false for the entry, as it should |

`Entry::always_drained` is true only when every backend of a chain is marked,
so a partial flag never refuses a chain that has a per-occurrence backend.

### To whoever owns `src/collect/mem/heap/`: a free that carries its size

The shim writes `bytes=0` on every free (`src/collect/mem/heap/alloc.rs`), so
no `closes` predicate can pick four 8 MiB frees out of the thousands of small
ones an interpreter makes. `examples/join-between-residency.yml` therefore
cannot credit the −32 MiB of its freed phase to anything, and says so. The shim
already tracks every live allocation with its size (`mem.heap.live` reads that
table), so:

```rust
// in the free record, where `bytes` is 0 today
Attr::uint("bytes", tracked_size.unwrap_or(0)),   // the size the allocation was made with
Attr::uint("freed_untracked", u64::from(tracked_size.is_none())),
```

would let a session write `closes: "op == 'free' && bytes >= 1048576"`.

### To stream R, once the lifetime is in the results: `whole: lifetime`

The exact whole of a time decomposition is the launched program's life. Until
`Results` (or `RunOutput`) carries it, `whole: window` is the only span, printed
as an upper bound. With stream R's `Lifetime { released_ns, exited_ns,
exited_err_ns }` in place, the engine change is one variant:

```rust
// src/join.rs
pub enum Span { Window, Lifetime }
// in `decompose`: amount = exited_ns - released_ns, resolution = exited_err_ns,
// kind = Kind::Window becomes a `Kind::Lifetime` that is a measurement, not a bound
```

`decompose` already receives the results, so the signature does not change.
Whoever lands the field can add the variant, or leave it to the join engine's
next owner.

### To whoever owns `man/omn.1`: `between` and `decompose:`

The manual's session section shows one `interval` join and no `decompose:`
block, and its EVIDENCE section defines `TEMPORAL` as "they overlapped in time",
which no match produced until now. Three things produce it since this wave:
`between`, `eq` on a recycled pointer, and `within_key` on an `eq` that replaced
states. `docs/families/join.md` has the fields `between` and `decompose:` take,
what each refuses, and the definition the engine applies: a correlation whose
only ground is the clock.

### To whoever owns `tests/aggregation.rs`: a float compared exactly

`two_emits_of_one_request_describe_the_same_events` asserts
`histogram.sum == summary.mean * summary.count` with `assert_eq!` on two
`f64`s. On this machine (Python 3.14.7, whose interpreter allocations are part
of the events) it fails every run, on `main` as on `feat/join-engine`, by one
ulp: `216167983.0` against `216167983.00000003`. The property holds and the
comparison does not: a mean times a count is not exact in floating point. The
bound is the rounding of one division and one product, so something like

```rust
let expected = mean * count;
assert!((sum - expected).abs() <= 2.0 * f64::EPSILON * sum.abs().max(1.0));
```

or asserting the summary's own `sum` field, if it carries one, exactly.

## Needs from another stream — the ninth wave, stream S (the shim, pointed elsewhere)

None blocking: `sched.mutex` and `gpu.api` both collect, with their
scenarios, and `docs/families/sched.md` and `docs/families/gpu.md` are the
record. Each item is a place the shim's sources are limited by a file this
stream does not own, or an answer to a request earlier waves addressed to it.

### To whoever owns `src/encode.rs`: a loss reason beside clean counters is never printed

`Losses::note` records a reason, and the text encoder prints the reasons only
when a counter is non-zero (`encode.rs`, the "no loss: every read succeeded"
branch). The heap stream's "no allocation was reported for pid ...: statically
linked, own allocator, never calls malloc" was a reason from the first wave
and has never once been printed. Found when `gpu.api` on a ctypes program
returned a silent zero. Worked around on this side -- the sentence is
`Collector::notes()` now, for all five preload collectors -- but any other
collector that notes an explanation without losing anything has the same
invisible sentence. Either print `reasons` whatever the counters say, or make
`note` on clean counters a debug assertion so the next one is found in a test.

### To whoever owns `src/capabilities.rs`: a probe for `dynamic-linking`

Every preload source -- `mem.heap.*`, `sched.mutex`, `gpu.api` -- requires
`dynamic-linking`, and nothing answers it, so every run of any of them prints
"unverified requirement". Whether the *target* is dynamically linked cannot be
known before it exists; whether this *build* can preload anything can:

```rust
"dynamic-linking" => match crate::collect::mem::heap::find_shim() {
    Ok(_) => Probe::Available,
    Err(reason) => Probe::Missing(reason),   // names the path and `make`
},
```

That also turns a `cargo build` without `--workspace` -- the silent failure
`CLAUDE.md` warns about -- into a `no` in `omn list` instead of a `?`.

### To whoever owns `src/resolve.rs`: the `hist` warning names a kernel a preload backend does not have

"`trace` with `hist` aggregates in userspace unless a budget lets the resolver
move the histogram into the kernel; consider --budget" is printed for
`sched.mutex` and `gpu.api`, whose only implemented backend is `preload`. No
budget moves anything there. The warning wants the same guard the pushdown
credit already has: only for a backend whose id begins with `bpf`.

### To stream B, for `catalog/mem.yaml`: three filter fields on `mem.heap.alloc`

Answering stream J's "a free that carries its size" (the eighth wave, above):
**done, under another name.** A free now carries `freed_bytes`, the size its
allocation was made with, or `freed_untracked: 1` when the allocation was never
seen. `bytes` stays zero on a free, because it is the source's value and every
sum and mean of `mem.heap.alloc` is taken over it; a free counted at its size
would double every total. So the predicate is

```
closes: "op == 'free' && freed_bytes >= 1048576"
```

`closes:` does not appear to be checked against `filter_fields`, but `--filter`
is, and `mem.heap.alloc` declares `[bytes, comm]`. Wanted:

```yaml
  filter_fields: [bytes, comm, op, freed_bytes, freed_untracked]
```

### To stream A's successor on `src/bpf/usdt.rs`: glibc's mutex probes, acknowledged

The seventh-wave note "To `sched`, for `sched.mutex`: glibc publishes the
mutex" is right, and it is the backend worth adding before the uprobe: it
reaches a process already running, which a preload never can. Not done in this
wave, which was the shim's. The catalog line would be this stream's; what it
needs from `usdt.rs` is to attach a library's probes (`libc:mutex_entry`,
`mutex_acquired`, `mutex_release`) in a pid that is already running, with one
program per site as it does today, and hand back `(tid, mutex, ts)` per hit.

### To the coordinator

- **Seven sessions** to add to `examples/README.md` and `GUIDE.md`; the rows
  are at the end of `docs/families/sched.md` (four) and `docs/families/gpu.md`
  (three). `examples/sched-not-implemented.yml` was rewritten, not added.
- **`OMN_HEAP_FD` is `OMN_SHIM_FD`**, and a new `OMN_SHIM_REPORT` names the
  classes. `docs/families/mem.md`, `docs/families/runner.md` and
  `docs/scenarios/cross-memory.md` mention the old name as history, which is
  still true of when they were written; nothing tests either name.
- **The shim record is 40 bytes**, not 32, with an `extra` field and a `flags`
  byte. Both sides assert the layout, as before.
- `STATE.md`'s "132 ns per event" for the shim is the heap's wave-two figure.
  For a mutex record it is about 415 ns in a release build, nearly all of it
  the one `write(2)` per record; `docs/families/sched.md` has the table.
---

## Needs from another stream — the ninth wave, stream B (memory by IBS)

`mem.latency` and `mem.sharing` collect through AMD IBS; `docs/families/mem.md`
has the measurements. Two requests outside this stream's files.

### To whoever owns `src/catalog.rs`: a cycle unit

`mem.latency`'s value is core cycles from the miss to the data. The catalog has
no unit for it, so the entry says `count` and the text output prints
`mean 316 count`. Converting to nanoseconds would go through a clock that moves
with load and temperature, which is a number the hardware did not measure.

```rust
pub enum Unit { /* ... */ Cycles }   // as_str: "cycles"
```

Stream B changes `unit:` on `mem.latency` the day the variant exists.

### To whoever owns `src/emit.rs`: an event whose value was not measured

On AMD family 17h an L1 hit is a sampled load with a level and no latency: the
unit times only loads that miss L1d. An event without its value attribute is
`EmitError::NotNumeric` in every emit, so `mem.latency` cannot emit those loads
and counts them by level in a note instead. Wanted: an event whose value
attribute is absent is counted as `unmeasured` by `hist` (beside `unbucketable`
in `SeriesHist`), `summary` and `topk`, rather than failing the emit, and `raw`
carries it unchanged. The level split would then be in the output rather than in
a sentence, and `topk --key mem` could rank the lines a walk touched including
the ones L1 answered.
## Needs from another stream — the ninth wave, stream P (the page cache)

Three requests, none blocking: `io.pagecache` and `io.readahead` collect and
their scenarios pass, and `docs/families/io.md` is the record.

### To whoever owns `tests/cli.rs`: a test that ran out of candidates

`a_valid_request_with_no_collector_names_the_ones_that_exist` looks for a
source with no collector that declares `trace`, `raw` and a system target, and
`io.pagecache` and `io.readahead` were the last two. What is left uncollected
declares other shapes, and streams S and B are collecting four more of them.
Choosing the mode and emit from the entry itself keeps the test about the
refusal rather than about which sources are left. There is no public mode-emit
predicate to ask, so the test asks the binary: every declared pair, the first
that the validator does not reject (exit 2), and then the assertions as they
are.

```rust
let candidates: Vec<(String, String, String)> = catalog
    .entries()
    .iter()
    .filter(|entry| {
        entry.backends.iter().all(|backend| {
            !omn::collect::implements(&entry.id.to_string(), &backend.id)
        }) && entry.target_support(omn::request::TargetKind::System)
            != omn::catalog::TargetSupport::No
    })
    .flat_map(|entry| {
        entry.modes.iter().flat_map(move |mode| {
            entry.emits.iter().map(move |emit| {
                (entry.id.to_string(), mode.as_str().to_owned(), emit.as_str().to_owned())
            })
        })
    })
    .collect();
let output = candidates
    .iter()
    .map(|(source, mode, emit)| {
        let mut args = vec![source.as_str(), "--system", "-m", mode, "-e", emit];
        if mode != "snapshot" {
            args.extend(["-d", "1s"]);
        }
        run(&args)
    })
    .find(|output| output.status.code() != Some(EXIT_REJECTED))
    .expect("this build does not collect every source in the catalog");
```

### To whoever owns `src/bpf/syscall.rs`: `map_update` outside `#[cfg(test)]`

The page cache programs keep each thread's last lookup in a plain hash map, and
userspace sweeps threads idle for a second. An entry the kernel refreshes
between the sweep's look and its removal is lost; with `map_update` it could be
put back. The signature is the one that exists, without the attribute:

```rust
pub fn map_update(map_fd: RawFd, key: &[u8], value: &[u8]) -> io::Result<()>
```

### To stream V, for after `bpf_task_storage` lands: the map it replaces

`src/bpf/io_pagecache.rs`'s `last` map is per-thread state that should die with
the thread, which is what task storage is. Once `map.rs` has it, the sweep and
the race above both go away; the program reads and writes four `u64`s at
offsets 0, 8, 16 and 24 of a thread's entry.
---

## Needs from another stream — the ninth wave, stream V (the vertical case)

Stream V's deliverable is `docs/scenarios/vertical.md`; these are what it needs
from files it does not own, each with the signature wanted.

### To whoever owns `src/join.rs`: a row read per `req` is IDENTITY

`Readout::row` writes `evidence: Some(Evidence::Key)` on every row of every
decomposition. `evidence(rule)` already rates a join on `req` IDENTITY -- the
downstream event carries what the upstream one put there -- and a
decomposition read `by: req` is read by that same propagated key, so its rows
print under-rated: every row of `examples/vertical-one-request.yml` says KEY.
Wanted, the same rule in both places:

```rust
// in Readout, carried from `decompose`
by: KeyKind,
// in Readout::row
evidence: Some(if self.by == KeyKind::Req { Evidence::Identity } else { Evidence::Key }),
```

### To whoever owns `src/join.rs`: a key the whole carries is a table

`decompose` builds its tables from the keys the *parts'* events carry, and
never from the whole's. `examples/vertical-one-request.yml`'s `block-io` table
has as its whole a request's uninterruptible wait and as its part the device
time of the block requests carrying the request's id; a request that waited
uninterruptibly and whose id no block request carries -- a page lock, a
metadata read in a kworker -- gets no table at all, where the right answer is
a table whose whole is its wait and whose residue is all of it. Wanted:
`per_key` extended with `whole_readout.keys()` when the whole is `Measured`.
`tests/vertical.rs` asserts today's absence, and will need the one `None` arm
changed when this lands.

### To whoever owns `src/join.rs`: seconded, an event without the key is unattributed

Stream T's need above (`missing_key` refuses a rule at the first `to` event
without the key) has a second producer. `net.request -> io.latency` on `req`
is refused at the first block request submitted outside a request -- the
server's own file creation, in this session -- which is every block request
that is not a request's. Wanted as T wrote it: counted as unattributed, with
its own count in the join's report.

### To whoever owns `src/join.rs`: a row that names who released it

The roadmap's tree prints `lock_wait ... WAKEUP -> holder tid=8821` and
`block_io ... IDENTITY -> 3 rq, dev 259:0`. The events carry both --
`net.request`'s `holder_tid`, and one `io.latency` event per request with its
`dev` -- and a row has no way to say either. Wanted, on `Part`:

```rust
/// An attribute whose value the row names beside its amount: the most
/// frequent one among the row's events for the key, with how many carried it.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub names: Option<String>,
```

and `Row::named: Option<(String, u64)>`, rendered after the share.

### To whoever owns `src/bpf/usdt.rs`: the uprobe-multi load and link are in `syscall.rs` now

`syscall::prog_load_for(prog_type, expected_attach_type, insns, name, license)`
and `syscall::link_uprobe_multi(prog_fd, path, offset, pid)` landed in this
wave, as stream U asked. `usdt.rs`'s `load_for_multi` and `link_multi` are
copies of them and can go; the semaphore offset is the one argument
`link_uprobe_multi` does not take yet, and `usdt.rs` needs it.

### To whoever owns `src/bpf/insn.rs`: three comparisons, one addition, four helpers

`src/bpf/request.rs` declares locally, as `fentry.rs` and `usdt.rs` already do
for some of the same: `add64_reg`, `jgt_reg`, `jge_reg`, `jne_reg` (all
`BPF_X`, unsigned), and the helper ids `PROBE_READ_USER = 112`,
`TASK_STORAGE_GET = 156`, `TASK_STORAGE_DELETE = 157`,
`GET_CURRENT_TASK_BTF = 158`. Three files now carry some of these; one copy in
`insn.rs` would be the one a test can hold.

### To the coordinator: two registration lines, and the indexes

A new file under `src/bpf/` and a new collector under `src/collect/net/` cannot
exist without a line in their module's registry, and neither registry is on
stream V's list: `src/bpf/mod.rs` gained `mod request;` and its `pub use`, and
`src/collect/net/mod.rs` gained `mod request;` and the `net.request` factory.
No other stream of the ninth wave owns either file. Two sessions were added,
`examples/vertical-one-request.yml` and `examples/vertical-abandoned.yml`;
their rows for `examples/README.md` and `GUIDE.md` are at the end of
`docs/scenarios/vertical.md`.

---

## The tenth wave's coordinator: what was answered on `main`

Answers to the ninth wave's requests, landed in files no stream owns this wave.

- **A cycle unit** (stream B): `Unit::Cycles`, spelled `cycles`, adds up like a
  duration, and `mem.latency` declares it.
- **An event whose value was not measured** (stream B): `hist`, `summary` and
  `topk` count an event without its value attribute as `unmeasured`, beside the
  numbers and moving none of them; an attribute that is there and is not a
  number is still `NotNumeric`. `src/collect/mem/ibs.rs` can now emit its L1
  hits as events with a level and no latency instead of a note.
- **A loss reason beside clean counters** (stream S): printed whatever the
  counters say, in the text encoder and the pprof comments.
- **The `hist` budget advice** (stream S): given only when the chain has both a
  written backend that buckets in the kernel and one that streams, which is the
  one case a budget can act on. It was previously given in exactly the case it
  could not: a chain with no kernel backend at all.
- **Three filter fields on `mem.heap.alloc`** (stream S): `op`, `freed_bytes`,
  `freed_untracked`.
- **`drained: true`** (stream J): on `io.syscall`, `io.fs`, `io.pagecache` and
  `io.readahead`. `catalog/net.yaml` is stream N's this wave and is in its brief.
- **`map_update` outside `#[cfg(test)]`** and **the page cache's map onto task
  storage** (stream P): the second made the first unnecessary. The per-thread
  state is a task-storage slot now; the sweep and its race are gone.
- **A test that ran out of candidates** (stream P): it now tries every source
  without a collector, every mode and emit its entry declares, and asserts on
  the first this machine can serve. Before, it picked `cpu.flow`, which this
  machine cannot serve, and returned without asserting anything.
- **`dynamic-linking`** (stream S) is stream X's this wave, with
  `src/capabilities.rs`.

### To stream T, for `man/omn.1`: two corrections from the fifth wave

The exit-status section still says exit 2 is "it cannot mean what it asks for,
**or this machine cannot serve it**" and exit 1 "usage, parsing, or a failure
while running". Since the fifth wave, a machine that cannot serve a valid
request exits **1** and a request that cannot mean anything exits **2**;
`GUIDE.md` under "The refusal" has the four-row table that is true. And `=~`
is listed among the filter operators without saying what it does:
`docs/fixes/emit.md` has the line wanted.

### To stream H, for `src/request.rs` and `src/collect/mod.rs`: three left from the fifth wave

`sched` 5, `count` refusing `topk` although a counted run has series to rank
(one arm of `ModeKind::valid_emits`); the counted window rounding up to the
runner's tick; and `what_a_run_reports_spending_is_the_cpu_it_actually_burned`
driving to a wall deadline instead of a cpu one. `docs/fixes/catalog.md` and
`docs/fixes/mem.md` have the reasoning. None blocks hot attach; the second is
close to what a per-request window touches anyway.

### For the flaky tally: two seen once under the tenth wave's load

Both failed once in a `--no-fail-fast` run while four stream agents were
building on this machine, and both passed alone on the first rerun. Neither is
in a path the change under test touched.

- `budget::what_the_tool_says_it_spent_moves_with_how_often_it_was_asked_to_read`
  -- a measurement of the tool's own cpu against a read interval, which a
  loaded machine moves.
- `scenario_sched::a_blocking_stack_names_the_call_that_blocked`.

### Answered: the kernel's skips, on every raw-tracepoint program

The eighth wave's "the kernel's skips are a loss nothing counts" is closed for
the fifteen programs it named: `runqlat`, `offcpu`, `wakeup`, `migrate`, the
four `sched_*` raw-tracepoint programs, `proc_syscall`, the three
`proc_events` streams, `mem_alloc`, `mem_stall`, and `sched_drm`'s classic
tracepoint pair through a new `PerfAttachment::program`. Each program has
`skipped()`, each collector reads it beside its drop counters into
`Losses::dropped`, and the reason says the count is the machine's.

It was not theoretical. The first run after the change counted skips in
`sched.lock` under `scenario_sched`'s contended workload, and a join that
folded 595 of 600 known wakeups -- which had passed with a fixed slack of three
-- now has a counted loss beside it; `tests/join_engine.rs` carries that count
as its slack. A unit test in `src/bpf/mod.rs` refuses any file that attaches a
raw tracepoint without a `skipped()`, which is the per-file test finding 46
asks for.

Still open from the same entry: the drain racing the program's increment in
`runqlat` and `proc_syscall`, which the two-map generation scheme of
`fentry::HistogramProgram` would close.

### To stream N, for `src/collect/net/`: two lines the tenth wave's decisions leave you

- **`net/dev.rs::stop` takes a closing read**, and `DECISIONS.md` ("the runner
  closes the window") settled that it should not: the runner's last round is
  the closing read, and the extra one covers tens of microseconds and emits a
  zero-delta point at the previous offset. `io` and `power` already refuse to
  read there. Wanted: `stop` returns what is buffered and reads nothing, and a
  scenario that no series ends in two points at one offset.
- **`net/capture.rs`'s refusal** names `CAP_NET_RAW`. Since the tenth wave
  `make CAPTURE=1` is what grants it (`DECISIONS.md`, "`CAP_NET_RAW` is
  opt-in"); the refusal should name that command beside the capability.

## Needs from another stream — the tenth wave, stream X (the bench)

Four defects the bench's ground truth found in files this stream does not own.
Each was met by a workload that knows its own count, and each has the one
command that reproduces it. None blocks the bench -- each is worked around in
`tests/bench_*.rs`, and the workaround is named there -- but three of them are
the silent degradation this project is built against. `docs/bench.md` has the
context.

### To whoever owns `src/filter.rs` and `src/collect/proc/`: `syscall == 110` drops every call it pushed down

```console
omn trace proc.syscall -m trace -e summary --filter 'syscall == 110' -- <anything calling getppid>
  degraded: only `getppid` (110) was timed: the predicate on `syscall` was pushed into the program ...
  degraded: the predicate removed 2000 calls here and kept 0
```

The number is pushed into the eBPF program correctly -- only getppid is timed --
and then a second, userspace pass evaluates the same predicate against the
`syscall` attribute, which is the *name*, and a string is never equal to 110.
Every call the kernel kept is dropped, and the result is an empty summary on a
workload that made 200 000 of them. `syscall == "getppid"` counts 200 000 of
200 000. Either the userspace pass compares a number against `nr`, or it is
skipped for a predicate the program already applied, or `syscall == <number>`
is refused with the spelling that works. Found by
`the_forced_backends_count_what_the_workload_did`.

### To whoever owns `src/collect/proc/events.rs` (or the filter's push-down): `proc.exec` accepts a filter and applies none

```console
omn trace proc.exec --system -d 1s -m trace -e raw --filter 'filename == "/nonexistent/path"'
```

reports every exec on the machine, `/bin/true` and `/usr/bin/env` included,
and says nothing about the filter: no degradation, no warning. The same with
`comm == "nosuchcomm"`. Both fields are in the catalog's `filter_fields`, so
the validator accepts them. A filter that is accepted and not applied is the
one outcome the invariants rule out: apply it, or refuse it with the reason.
Found when a count of 200 execs came back 266 on a shared machine.

### To whoever owns `src/resolve.rs`: `--backend force:` walks past a backend's missing probes

```console
omn trace mem.cache_llc --backend force:pmu -m count -e summary -- true
error: cannot start collecting `mem.cache_llc`: perf_event_open ... refused (No such file or directory)
```

`omn list mem.cache_llc` says both of `pmu`'s requirements are missing on this
cpu (`LLC-load-misses` and `LLC-loads`, which Zen 2 does not publish), and
`--dump-resolved` with the backend forced carries those two `missing` probes
in the file it prints -- and accepts the request. The run then launches the
target and fails at attach, which is finding 15's shape again, reached through
the pin instead of the probe. A pinned backend whose requirements probe
missing should be refused before the launch, with the probes' reasons; the
bench now reads them out of the resolved file itself to avoid the launch.

### To whoever owns `catalog/mem.yaml` or `src/collect/mem/page_faults.rs`: `count` mode is declared and refused

`mem.faults` declares `count`, the validator accepts `-m count`, and the
collector refuses it after the launch: *"`count` mode does not sample page
faults"*. Either the catalog stops declaring the mode for `pmu:sw`, or the
collector counts -- a `PERF_COUNT_SW_PAGE_FAULTS` counting event is the
cheapest thing it could do. The bench's plan now tries each accepted setting
once and moves to the next when a collector refuses it, and lists every such
refusal in its table's "Not measured here".

### To the coordinator

No registration lines and no sessions: the bench adds no module, no collector
and no `examples/bench-*.yml` session, so the index test is not affected by
this branch. `docs/bench.md` is the handoff and ends with the one command for
the real measurement.

## Needs from another stream — the tenth wave, stream N (the network rows)

Stream N's deliverable is `docs/scenarios/vertical.md`; this is what it needs
from a file it does not own, with the signature wanted.

### To whoever owns `src/join.rs`: a row read through another key is rated by it

`Readout::row` rates every row of a decomposition by the decomposition's `by`
alone: `by: req` is `IDENTITY` for every row. `examples/vertical-one-request.yml`
now has two rows that are not read by the request's id. `net_rx` and `net_tx`
are measured by programs that never run in the serving thread -- the first
byte is queued in softirq, the last leaves when the peer's acknowledgement
opens the window -- and what ties them to the request is the socket's cookie,
matched between those programs and the probe that named the socket. That is
`KEY` by the roadmap's own table, and today the tool prints both rows
`IDENTITY`: over-rated, which is the one direction the ladder must not err in.
Wanted, on `Part`:

```rust
/// The key the row's value was carried through before it reached the
/// decomposition's `by`: `sock` for a row the socket's cookie tied to the
/// request. The row is rated the weaker of the two keys.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub via: Option<KeyKind>,
```

and in `Readout::row`, with `rate(KeyKind::Req) == Identity` and every other
kind `Key`, as today:

```rust
evidence: Some(match self.via {
    Some(via) => rate(self.by).min(rate(via)),
    None => rate(self.by),
}),
```

When it lands, two lines of `examples/vertical-one-request.yml` take
`via: sock` (the `net_rx` and `net_tx` parts), and
`tests/vertical.rs::the_network_rows_are_read_by_the_socket_the_server_accepted`
flips its one `IDENTITY` assertion to `KEY`; both are marked there.

## Needs from another stream — the tenth wave, stream H (hot attach)

A request's `window` (`1s..3s`, `1s..`, `..2s`) is attached and detached by
the runner while the rest of the run goes on; every `Collection` carries the
span it watched as `collect::Watched`. `docs/scenarios/attach.md` is the
record. What that layer cannot finish from its own files:

### To whoever owns `src/join.rs`: a `to` event outside the `from` source's window

Measured, not assumed (`tests/runner.rs`,
`a_join_across_a_detached_source_does_not_count_what_fell_after_it_as_a_loss`,
ignored until this lands). `sched.wakeup` detached at 1 s, `sched.switches` for
the whole run, a program with 40 sleeps before the detach and 25 after: the
join says **67 matched, 0 unmatched**. The last wakeup it saw before the detach
stays open, and every block after the detach is credited to it
(`task:0 (idle) -> task`, 63 matches, `still_open`). That is worse than a loss:
it is a stale state standing in for wakeups the run stopped watching. For
`interval` and `eq` the same shape holds -- a range opened before the detach
can no longer be closed, so it matches forever.

Wanted, on `Correlation`:

```rust
/// Downstream events stamped outside the `from` source's window: before it
/// was attached or after it was detached. Neither matched against a state
/// nor counted as unmatched -- the run was not watching for what would have
/// opened or closed one.
#[serde(default, skip_serializing_if = "is_zero_u64")]
pub outside_window: u64,
```

and an entry point that takes the windows, with `apply` kept as the whole-run
case:

```rust
/// `apply`, knowing when each source was watching. `windows` maps an event
/// kind (a source id) to the span its collection reports; a kind absent from
/// it was watched for the whole run.
pub fn apply_within(
    rule: &Rule,
    events: &[Event],
    windows: &BTreeMap<String, crate::collect::Watched>,
) -> Result<Correlation, JoinError>;
```

Inside it: a `to` event with `!windows[rule.from].contains(ts)` goes to
`outside_window` and is not looked up; states still open when the `from`
window closes are closed there, and counted apart from `expired`. Two requests
of one source with different windows are the one case a map by kind cannot
say; refuse that rule by name rather than taking the union.

### To whoever owns `src/main.rs` (stream T this wave): pass the windows to the join

In `measure`, where `join::apply(rule, &merged)` is called: build the map from
the collections before they are consumed --

```rust
let windows: BTreeMap<String, collect::Watched> = collections
    .iter()
    .zip(resolved)
    .map(|(collection, resolved)| (resolved.request.source.to_string(), collection.window))
    .collect();
```

-- and call `join::apply_within(rule, &merged, &windows)` once it exists.

### To whoever owns `src/emit.rs`: the window on `Results`

`Collection::observed_ns` is already the request's own span, so `rate` is
already over the window (a source attached for two seconds of ten has a rate
over two). What a document cannot say yet is *where* the window was. Wanted,
on `Results`, copied in `aggregate` from `collection.window`:

```rust
/// When this request was watching, inside the run: its own window when the
/// request carried one, the run's otherwise.
#[serde(default)]
pub window: crate::collect::Watched,
```

Until it lands the placement reaches the reader only as a note -- "watched
only from 1.500 s into the run, as its window asked: attaching it took 26.5us
while the target ran" -- which `render.rs` prints under `degraded:`, and as the
collection's `stopped_because` -- "its window closed: detached 2.000 s into the
run, as asked".

### To whoever owns `src/collect/cpu/counters.rs`: say what a late counter cannot see

The largest thing this stream found. A per-task counter follows a process by
inheritance, and inheritance is from the open: attached at the launch gate it
follows every thread the program will ever start, and attached by a window to
a program that already runs a second thread it counts the thread it was
opened on and nothing else. Measured: a window over a phase that retires
2,000,000,000 instructions on libtest's test thread counted **106,206**, with
no loss and no note (`tests/runner.rs`,
`a_counter_attached_late_counts_the_threads_already_running_or_says_it_cannot`,
ignored until this lands).

The runner already acts on a declaration: `Prepared::follows_only_later_threads`
(in `src/collect/mod.rs`, default `None`). When it answers, and the target has
more than one entry in `/proc/<pid>/task` at the attach, the window is not
attached and its loss says why. Wanted, on `counters.rs`'s `Prepare` (and on
any other per-task perf collector that inherits -- `cpu.profile`,
`sched.switches` and `mem.faults` are worth checking):

```rust
fn follows_only_later_threads(&self) -> Option<&'static str> {
    Some("a per-task counter follows a process by inheritance, from the moment it is opened")
}
```

The better fix is in the collector: open one counter per entry of
`/proc/<pid>/task` at the attach, and inherit from each. Then the declaration
goes, and the ignored scenario passes on its first branch.

### To whoever owns `src/cli.rs` (stream T this wave): `--window`

The session file has it; the command line does not. In the request builder at
`cli.rs:377`, which now carries `window: None`:

```rust
/// When, inside the run, this request is attached and detached: `1s..3s`,
/// `1s..` or `..2s`. Absent is the whole run.
#[arg(long, value_name = "START..STOP")]
window: Option<omn::request::Window>,
```

and `window: args.window` in the literal. `Window` implements `FromStr` with
the refusals a session file gives (a window that closes before it opens, `..`
on its own), so the two surfaces refuse the same things in the same words.
`man/omn.1` would want the same line.

### To everyone who writes a `Request` literal: `window: None`

`Request` gained `window: Option<Window>`. The five literals outside this
stream's files -- `src/cli.rs`, `src/resolve.rs`, `src/render.rs`,
`src/validate.rs`, `tests/catalog_model.rs` -- got one line each, `window:
None`, because the build does not compile without it; nothing else in those
files changed. Stream T's new `src/triage/` builds requests and will need the
same line in each literal it adds.

### To the coordinator: `DECISIONS.md`'s concurrent-requests line, and the indexes

The line *"One request per run, for now -- a session with several is refused
... concurrent attach is the honest fix, and it lands with hot attach"* has not
described the code for several waves: a session runs any number of requests in
one window, one workload beside any number of machine targets, and this wave
gives each its own window inside it. What is still refused is two
*workloads*, and hot attach does not change why -- whose exit ends the run,
which program a launch means, and what an overhead is a percentage of each
still have two answers. The line wants rewriting to say that. The two sessions
this stream added have their rows at the end of `docs/scenarios/attach.md`.

## Needs from another stream — the tenth wave, stream T (triage)

None blocking: `omn latency`, `omn saturate` and `omn diff` answer with their
scenarios, and `docs/triage.md` is the record. Each item is a place the layer
above the engine worked around a file it does not own, or something the
coordinator has to land beside the merge.

### To whoever owns `src/join.rs`: a filtered part that matched nothing is a zero, not a missing row

`omn latency` wanted to split the blocked row by task state inside the engine,
as three parts of one `sched.offcpu` request with `filter: "state ==
uninterruptible"`, `"state == sleeping"` and the rest. It works, and every part
the task had no block of comes back `not read` -- "no `sched.offcpu` event
carried both `total_ns` and `queued_ns`" -- which makes `parts` and `RESIDUE`
`not read` for every task that did not block in all three states, which is
every task. The source ran, was clean, and had events for the key; none of them
passed the filter. That is a measured zero, and the row can say how it knows:

```rust
// in Readout, per key: the source produced events for this key and the
// part's filter kept none of them
Row { amount: Some(0.0), note: Some("no event of this key passed the filter".into()), .. }
```

The same holds, more weakly, for an unfiltered part of a drained source that
ran clean from the gate and saw nothing of a key another part did see: a
cpu-bound task that never blocked has no `sched.offcpu` event, and its blocked
row is `not read` where it is zero. Whether a source attached before the gate
may claim that is the join engine's call; the filtered case is not in doubt.

Worked around here: the engine decomposes on-cpu, runnable and blocked, and
`src/triage/latency.rs` splits the blocked row by state from the same events
with the same arithmetic, so the states still add up to the row.

### To whoever owns `src/collect/cpu/` or `src/perf.rs`: a profile that follows threads

`cpu.profile` on a launched program samples the task it launched and no thread
it starts ("a sampling event is not inherited"), so `omn latency` reads the
main thread's on-cpu time exactly and every other thread's as nothing. The
answer is about the main thread and says which task it is, so nothing is
misattributed -- but the other threads are listed with their on-cpu row
reading zero. `perf_event_attr.inherit = 1` is allowed for a sampling event
into a per-task ring as long as the group is not read with
`PERF_FORMAT_GROUP`; whether the ring and the symbolizer then cope with a
second task is this request.

### To the coordinator: the fixture in `make`

`examples/triage-*.yml` launch `target/omn-triage-fixture/workload`, which
`tests/triage.rs` builds from `src/triage/workload.c`. So the sessions run after
`cargo test --test triage` and not after `make` alone, which is the rule the
vertical-case server already follows. A target like `vertical-server`, same
shape, never fatal:

```make
TRIAGE_FIXTURE = target/omn-triage-fixture/workload

triage-fixture:
	@mkdir -p $(dir $(TRIAGE_FIXTURE))
	@if cc -O1 -g -fno-omit-frame-pointer -mno-omit-leaf-frame-pointer -fno-inline -pthread \
	      -o $(TRIAGE_FIXTURE).partial src/triage/workload.c 2>/dev/null; then \
	    mv $(TRIAGE_FIXTURE).partial $(TRIAGE_FIXTURE); \
	else \
	    rm -f $(TRIAGE_FIXTURE).partial; \
	    echo "note: the triage fixture did not build (no cc); examples/triage-*.yml need it"; \
	fi
```

and `@$(MAKE) --no-print-directory triage-fixture` at the end of `build`. The
flags are the test's: frame pointers in leaf functions too, because
`omn diff` names a function by its leaf frame.

### To the coordinator: `=~` in the manual, and the assertion that waits for it

`man/omn.1` is stream T's this wave, and the ninth wave's request to say what
`=~` does in it is ready -- but
`tests/scenario_runtime.rs::defect_2_the_match_operator_is_a_substring_test_and_says_so`
asserts the manual is still silent, and that file is not T's. Landing one
without the other turns the suite red, so both go in one commit at the merge:
the troff text is in that request above (search "the remaining half of runtime
2"), and the assertion to drop is the last one of that test.

### To the coordinator: `DECISIONS.md`

Two lines worth recording as settled, both enforced by `src/triage/objective.rs`
and its tests: a triage request without an objective is refused with the list,
never defaulted; and a method refuses an objective it does not answer for and
names the one that does (`omn latency` answers latency and tail, `omn saturate`
throughput, tail and energy). `cost` is not an objective: nothing here measures
a price.

---

## 47. What the bench's first real run measured, and what it corrects

`docs/bench.md`, "The table": 94 (source, backend) pairs, alone on the
development machine after the tenth wave. The resolver's seven choices between
served backends were all confirmed. What the table corrects is elsewhere, and
each item is a request to whoever owns the file this wave or the next.

- **The cost bands describe the tool's cpu, and an in-kernel collector's cost
  is in the target.** `proc.syscall` in `count`: 0.42 % of the target's cpu in
  `cost.json`, **+296 %** in the target's own timed section. The program runs
  on every syscall in the target's context. `io.fs` +7.5 %, `sched.futex`
  +5.8 %, `io.syscall` +3.8 %. Wanted: `cost.json` and the `--budget` ladder
  read the dilation where the run has it (a launched target that times
  itself does), and the catalog's bands for per-event raw-tracepoint backends
  reconsidered against the column. To whoever owns `src/collect/mod.rs` (the
  budget) and the catalogs.
- **The preload shim on an allocation-bound program: about +15 500 %.** A
  million allocations, 7 ms bare, about 1.1 s under each of the three heap
  sources -- the two that only poll included, so the cost is the interposed
  call and its record, not the emit. About 1.1 us an allocation, against the
  132 ns finding 6 measured in isolation, which the code and the catalog still
  quote in places: corrected where no stream owns the file, and
  `src/collect/sched/mutex.rs` and `mutex_usdt.rs` at the eleventh wave's merge.
- **The preload shim under a lock storm: +1043 %, 3.9 GiB, 54.9 million
  records lost.** `sched.mutex` on four threads doing a million round trips
  each. The losses are counted; the memory is the tool's own, buffering what
  it did read, and the dilation is one `write(2)` per record into one pipe,
  which the observer step independently measures as a serialisation (four
  allocating threads scale 1.96 under the shim against 1.01 bare). Wanted: a
  bound on what the shim stream buffers before it drops and counts, and a
  batched write in `shim/`. To whoever owns `shim/**` and
  `src/collect/mem/heap/**`.
- **Attach dominates a short fentry run**: `io.fsync` and `net.tcp.connect`
  spent 64 and 68 ms with no event, the trampoline rebuild. Honest as a
  number; worth a line in the cost band's caveat. To whoever owns
  `catalog/io.yaml` and `catalog/net.yaml`.
- **NVML's power query costs 34 ms a read** (`gpu.power`, 21 % of the target at
  100 ms), `gpu.freq` 15 ms. The default poll for those two may want to be
  slower than the family's. To whoever owns `catalog/gpu.yaml`.
- **`runtime.jit` reports 360 losses on a run with no event.** Not diagnosed.
  To whoever owns `src/collect/runtime/jit_map.rs`.
- **Tracing a contended program shortens it**: `sched.lock` −21 %,
  `sched.offcpu` −11 %, `sched.runqlat` −9 % on the contended workload. Not a
  defect -- the observer effect the roadmap asks the bench to measure -- and
  worth a caveat on the three sources: a lock measurement describes the run it
  perturbed. To whoever owns `catalog/sched.yaml`.

### Answered on `main`: the shim writes into a shared ring

The shim's record cost was two system calls, `gettid` and `write(2)`, and they
were all of it: on a million allocations, interposing without reporting took
14 ms against 7 ms bare, reporting to `/dev/null` 767 ms, and reporting into a
drained pipe 1133 ms. Since then `omn` maps a 3 MiB ring in a `memfd` and hands
it over (`OMN_SHIM_RING_FD`); a record is a compare-and-swap reservation, a
copy and a release store, and the thread id is read once per thread (reset
after `fork`). The pipe remains the fallback, and a full ring drops and counts
in its header, which the reader turns into the drop record it always read.

Measured on the same workload, release shim and release `omn`: the program
runs in **90 ms instead of 1.40 s** -- about 70 ns a record -- and every record
is still accounted for (196 608 events and 1 803 397 counted drops of 2 000 005).
What changed is who is slow: the program now produces thirty million records a
second, and `omn` reads about seven, so a storm like this one is mostly counted
drops where it was mostly a program slowed to a crawl. That is the trade the
invariants ask for -- the target is not changed to be measured, and what was
not measured is said -- and the reader's own speed (about 1.3 us a record in a
debug build, the event model's allocations) is the next limit, not the shim's.

Two things this leaves for others. The ring's pages are mapped in the target,
so up to 3 MiB of a `mem.rss` read beside a preload source is the ring: a
caveat for `catalog/mem.yaml` (stream S's this wave) and `catalog/sched.yaml`.
And the bench's heap rows in `docs/bench.md` predate the ring; one rerun of
`OMN_BENCH_ONLY=mem.heap scripts/bench.sh backends` replaces them.

### For the flaky tally: two more, seen at load 20 during the eleventh wave

Both failed once in a `--no-fail-fast` run while the four stream agents ran
their own suites (load average 20 over 24 cpus), and both passed alone on the
first rerun:

- `per_cpu_utilisation_reports_one_series_for_each_core_asked_for`
- `what_the_signal_ring_could_not_hold_is_counted_and_closes_against_what_was_sent`

The same run also failed `collect::tests::what_a_run_reports_spending_is_the_cpu_it_actually_burned`
(power 5, already in the tally) and `an_overflowing_wakeup_ring_says_what_it_could_not_hold`
(`docs/fixes/stacks.md`, already there).

## Needs from another stream — the eleventh wave, stream J (the engine's debts)

Stream J's deliverable is `docs/families/join.md`, "The eleventh wave". The
four requests the tenth wave filed against `src/join.rs` and `src/emit.rs`
have landed; what is left needs a file stream J does not own.

### To stream E (`src/triage/latency.rs`): one line, already made

`join::Part` gained `via: Option<KeyKind>` (stream N's request). Rust has no
way to add a field without breaking every struct literal, and
`src/triage/latency.rs:186` builds `Part` with one, so the branch does not
compile without `via: None,` after `names: None,` there. That single line is
the only edit on this branch outside stream J's paths. If E's branch rewrites
that closure, keep the line; if it removes the literal, drop it.

Worth knowing for `omn latency`: a filtered part that matched nothing is now a
zero (`amount: 0`, note `no event of this key passed the filter`) instead of
`not read`, so the three-part split by task state that
`src/triage/latency.rs` works around can be done inside the engine now.

### To the coordinator (`tests/runner.rs`): un-ignore the detach scenario, with one bound changed

`a_join_across_a_detached_source_does_not_count_what_fell_after_it_as_a_loss`
is ready. Six runs with `--ignored` on this branch gave `outside_window: 25`
every time, `unmatched: 0` every time, and `matched` of 43, 44, 42, 41, 43
and 44 (on `main` it is 67 matched). The last assertion,
`matched <= FIRST + 3`, failed on the 44 once: that run had 44 wakeups inside
the window against 40 sleeps, and each extra wakeup comes with its own
switch. Those extra wakeups come from the load the other streams put on the
machine, not from the engine. The bound that holds the separation without
calibrating to the load is:

```rust
assert!(
    matched < FIRST + SECOND && unmatched < SECOND,
    "what fell after the detach is neither matched nor unmatched: {join}"
);
```

The defect credited all 65 blocks, and the fix credits none of the 25, so the
margin is the 25 blocks the scenario exists to check.

### To the coordinator: `tests/scenario_runtime.rs` is not `cargo fmt`-clean on the base

`cargo fmt` rewrites one `assert!` at `tests/scenario_runtime.rs:929` on
`9317a3d`. That file is not stream J's, so this branch leaves it unformatted,
and `make check`'s final `cargo fmt --check` flags it here. It will flag it on
every branch cut from that commit.

## Needs from another stream — the eleventh wave, stream E (the escalation)

None blocking: a session with an `escalate:` block and requests held with
`window: on <layer>` runs through `omn -f` today, attaches the layer its cheap
pass names, and holds its budget, with its scenarios in `tests/triage.rs`.
`docs/triage.md` is the record. What it would say better with a file it does
not own:

### To whoever owns `src/main.rs` (stream J this wave): a held request that does not resolve is a layer this machine cannot serve, not a failed run

`trace` resolves every request of a session and returns at the first
`ResolveError`. For a request held for the escalation that is the wrong answer:
the session asked for it only in case its layer were named, the cheap pass
does not need it, and the probes' reasons are exactly what the escalation
should print when it names that layer. Today such a session exits 1 before
anything runs. Wanted, in the resolution loop:

```rust
// A request held for the escalation (`window: on <layer>`) that this machine
// cannot serve is not a reason to refuse the run: set it aside with the
// resolver's reason, and the escalation reports it if it names that layer.
let held = request.window.is_some_and(|window| window.on.is_some());
match resolve::resolve(&catalog, &capabilities, request) {
    Ok(resolved) => resolutions.push(resolved),
    Err(error) if held => unserved.push(omn::collect::Unserved {
        request: request.clone(),
        reason: error.to_string(),
    }),
    Err(error) => { /* as today */ }
}
```

and `collect::run_all_within_unserved(&resolutions, &mut ladder, &unserved)`
in `measure`, which this stream will add beside `run_all_within` once the call
site is agreed -- the policy already reports a named layer whose held requests
could not be attached, with each one's reason (`src/triage/escalate.rs`,
`close`). Until then a held request that does not resolve refuses the run with
the resolver's own message, which is honest and louder than it needs to be.

### To whoever owns `src/join.rs` (stream J): lift one refusal when `apply_within` lands

`Session::check_escalation` (`src/request.rs`) refuses a join or a
decomposition that reads a held source, because a state opened before the
escalation detaches its source would absorb every event after it -- the shape
stream H measured at 67 matches for 40. Once `join::apply_within` closes states
at the `from` window's end and counts `outside_window`, and `main.rs` passes
the windows, `nothing_reads_across_a_held_window` can go for joins. A
decomposition's parts would also need weighing against their own window rather
than the run's whole; that half stays.

### To whoever owns `src/emit.rs` (stream J): the window on `Results`, seconded

The escalation's documents say where each held request was watched in words --
"attached 1.413 s into the run by the escalation ... (CLOCK_MONOTONIC a to b
ns)", "detached 1.481 s into the run by the escalation: ..." -- and the
scenarios parse the monotonic stamps out of the note. `Results::window`, copied
from `Collection::window`, would make that a field; the notes stay, since they
carry the *why*.

### To the coordinator: `DECISIONS.md`, the escalation's mechanism

Worth recording as settled, and enforced by `src/collect/mod.rs`'s tests:

- The runner asks `collect::Escalate` once a round, after its reads and after
  the stop condition, and applies the answer detaches first. It never learns
  what a layer is: the one line of the collector layer that names triage is
  `escalation_for`, the registration, as `registry()` is for the families.
- An escalation can only attach requests the session declared, held with
  `window: on <layer>`, validated, resolved and prepared before the launch. It
  cannot reach a backend the resolver did not choose.
- A held request is attached at most once: a collection has one window.
- A held request over its budget with nothing cheaper is let go, not the end
  of the run; a request attached mid-run is weighed once it has run for one
  budget check interval.
- The objective is required (`escalate: {objective: latency}`), and only
  `latency` is decided for.

### To the coordinator: the index rows

One session, `examples/attach-escalate-phases.yml`; its rows are at the end of
`docs/triage.md`.

### To the coordinator: `tests/scenario_runtime.rs` is not formatted on `main`

Line 926, `assert!(described, "the manual must say that `=~` is a substring
test");`, is one line where rustfmt wants three, so `cargo fmt --check` -- the
last step of `make check` -- fails on `main` and on every branch made from it,
and a stream that runs `cargo fmt` before `make`, as the brief asks, rewrites a
file it does not own. `cargo fmt` once on `main` closes it.

## Needs from another stream — the eleventh wave, stream K (drains that lose nothing)

`src/bpf/map.rs` now has `Rows`: two row maps behind a one-slot
`BPF_MAP_TYPE_ARRAY_OF_MAPS`, a drain that swaps the slot, waits for a grace
period with `membarrier(MEMBARRIER_CMD_GLOBAL)` and empties the map it retired.
Ten programs in `sched`, `proc` and `mem` use it; `docs/families/drains.md` has
the measurement (17 to 25 calls lost per run on `main` for a program that counts
its own, none after). Three drained programs outside this stream's files still
have the race, and one has the lagging version of the fix with a grace period
this kernel does not honour.

### To whoever owns `src/bpf/io_pagecache.rs`, `net_qdisc.rs` and `net_softirq.rs`: the same race, the same fix

Each drains a row map with lookup-and-delete while its programs look a row up
and add to it. Moving one is four lines and a test:

```rust
use super::map::{row_map, select_rows, Rows};

let rows = Rows::create(ROW_KEY_BYTES, ROW_VALUE_BYTES, MAX_ROWS, "omn_x_rows")?; // was Hash::create
// before the first row lookup, once per run; ROWS_SLOT is an 8-byte stack slot
select_rows(&mut program, rows_fd, ROWS_SLOT, "no_rows");
program.push(row_map(1, ROWS_SLOT));          // replaces each load_map_fd(1, rows_fd)
// and a "no_rows" label that counts a row drop, which is never reached while
// the slot holds a map

#[test]
fn the_row_map_is_taken_from_the_slot_once_per_run() {
    crate::bpf::map::assert_rows_taken_once(&program, rows_fd);
}
```

`drain()` keeps its signature and its meaning -- every row since the last
drain, nothing a tick late -- so no collector changes. Add `ROWS_SLOT` to the
file's stack-region test.

### To whoever owns `src/bpf/fentry.rs`: the last drain's 1 ms is not a grace period here

`HistogramProgram::drain(last: true)` flips the word and sleeps a millisecond
for "a call that read the generation before the flip". On this kernel
(`CONFIG_PREEMPT=y`, 7.2) BPF programs are preemptible: `proc.syscall`'s
`sys_exit` program, which no interrupt re-enters, was skipped by the kernel's
recursion guard 4300 and 15829 times in two runs of eight busy threads, which
only happens when a run was preempted mid-program on its cpu. A preempted
exit half can hold the retired map far longer than 1 ms. Either
`membarrier(MEMBARRIER_CMD_GLOBAL)` in place of the sleep (5 to 18 ms here, and
exact), or moving to `map::Rows`, which also removes the tick of lateness. The
ordinary drains, which wait a whole interval, make the same bet with a hundred
times the margin; it is still a bet.

### To stream E, for `src/collect/mod.rs`: one grace period per round, not per source

`Rows::drain` sleeps 5 to 18 ms for its grace period, in the runner's thread.
One drained source never notices. Six drained sources in one session, drained
one after another, can push a 100 ms round past its tick, which the runner then
counts as `late_ticks` -- honest, but avoidable. A two-phase drain would pay
once per round:

```rust
// in map.rs, beside Rows::drain
pub fn retire(&self) -> io::Result<usize>;      // swap only; returns the retired side
pub fn take(&self, retired: usize) -> io::Result<Drained>;
pub fn grace_period() -> io::Result<()>;       // made pub
```

and a `Collector::retire(&mut self)` the runner calls on every drained
collector of a round before one `grace_period()` and then the ordinary
`sample()`. Not done here: it is a change to the collector trait and the round
loop, which are E's this wave, and nothing is wrong without it.

### To whoever owns `src/collect/mem/` (IBS) and `tests/scenario_mem.rs`: a `stack` over `mem.latency` fails one run in ten, as a machine limit

`omn mem.latency -m sample@997hz -e stack -- /bin/sh -c "sleep 0.05"` exits 1
about one run in ten, on `main` as on this branch, with

```
error: `latency_cycles` is not a numeric attribute, so it cannot be aggregated
```

Exit 1 says the machine cannot serve the request, which is not what happened:
some run's events carry `latency_cycles` as something other than a number,
most likely when an IBS op had no latency to report. It is what makes
`scenario_mem::every_mode_and_emit_of_the_sampled_sources_is_served_or_refused_by_the_right_code`
flap -- `["mem.latency sample stack"]` against `[]` -- and it failed in both of
this stream's full runs, once alone. Not investigated further here: none of
it is in this stream's files.

### For the flaky tally: seen once in a full run, passed alone at once

`collect::tests::weighing_a_run_costs_a_fraction_of_the_run` (12.3 ms of a
605 ms window), `scenario_sched::a_workload_that_is_half_blocked_measures_as_half_blocked`
(the `sched.wakeup` half: 1584 wakeups against 1200 +-25 %; the off-cpu half,
the drained one, passed), `scenario_net::every_segment_the_kernel_resent_is_one_this_run_saw`
(4 against 3), `scenario_proc::what_the_signal_ring_could_not_hold_is_counted_and_closes_against_what_was_sent`
(6 late ticks of a 10 ms tick), and `join_engine::the_resident_size_decomposes_into_live_and_retained_with_the_residue_the_program_predicts`
(118 644 bytes over the interpreter allowance). None of them reads a drained
program.

## Needs from another stream — the eleventh wave, stream S (the silent degradations)

Stream S's deliverable is `docs/fixes/silent.md`. Three requests it found in
files it does not own, each the shape it was sent to close: a request accepted
and answered as a different question.

### To whoever owns `src/collect/sched/` (stream K this wave): three sources apply no filter

```console
omn sched.offcpu --system -d 300ms -m trace -e summary --filter 'comm == "nosuchcomm"'
```

reports every task on the machine, and says nothing. `sched.wakeup`,
`sched.offcpu` and `sched.migrations` declare filter fields, the validator
accepts a predicate over them, the resolver credits their `bpf` backend with
applying it in the program, the runner then leaves their events alone -- and
their factories never hand the collector the filter. `proc.exec`, `proc.exit`
and `proc.signals` had exactly this and are fixed in
`src/collect/proc/events.rs` (a `Predicate` applied per drained record,
counted, and said). Wanted, in `src/collect/sched/mod.rs`:

```rust
Factory::new("sched.wakeup", "bpf:raw_tp", |resolved| {
    Ok(Box::new(wakeup::Prepare {
        mode: resolved.request.mode,
        filter: resolved.request.filter.clone(),
    }))
}),
```

and the same for `offcpu` and `migrations`, each collector applying it the way
`runqlat` already does, with the note that says it ran here.

### To whoever owns `src/collect/sched/switches.rs`: a sampling event that follows threads

`sched.switches` opens `SampledEvent::open_software(.., Scope::Process(pid), ..)`,
which is not inherited, and discloses it ("a sampling event is not inherited").
`src/perf.rs` has the thing that is now: `ThreadSampler`, one inherited event
per thread per cpu sharing a ring per cpu, used by `mem.faults`. (`cpu.profile`
uses `TaskSampler` instead, because an inherited cpu-clock event loses the
unfinished part of a period on every cpu a task visits; `docs/fixes/silent.md`
has the measurement. A switch sampled at period one has no unfinished period.)
The replacement is two lines:

```rust
let spec = SampleSpec::software(perf::PERF_COUNT_SW_CONTEXT_SWITCHES as u64, 1, Callchain::Off);
let event = ThreadSampler::open_process(&spec, self.pid, RING_PAGES)?; // per cpu, not total
```

`drain`, `enable`, `disable`, `undecoded` have `SampledEvent`'s signatures;
`threads()` and `unlisted()` say what was followed, and `profile.rs`'s notes
are the wording.

### To whoever owns `src/catalog.rs`: where a predicate actually runs

`resolve::placement` says `"placement": "kernel"` for every backend whose id
starts with `bpf`, and most `bpf` collectors apply the predicate in userspace
over what they drained (they say so in the run's degradations; the resolved
file cannot). Wanted, per backend, what the program carries:

```yaml
- {id: "bpf:raw_tp", ..., filter_in_program: [syscall]}   # the fields pushed down
```

and `placement()` then reports `Kernel` for a predicate over those fields
alone, `Userspace` otherwise -- a change to `src/resolve.rs` once the field
exists. Stream S did not guess the list from the collectors' code.

---

## 48. A wakeup made from an interrupt is credited to the task it interrupted

Found chasing `a_workload_that_is_half_blocked_measures_as_half_blocked`,
which failed in four full runs and passed alone. `sched.wakeup` keeps a wakeup
when its waker or its wakee belongs to the target, and takes the waker from
`bpf_get_current_pid_tgid` -- which, for a wakeup performed from a hard or soft
interrupt, is whatever task the interrupt landed on. On a machine running a
full suite, the interrupts landing on the target's cpu wake disk and network
and timer work, and every one of those wakeups is attributed to the target as
its waker.

Reproduced on purpose: the half-blocked workload alone gives 1208 wakeups; with
loopback datagrams and direct I/O raising interrupts beside it, 1384 -- the
driver and the responder still at 605 and 602, and the extra 180 spread over
55 wakees outside the target. A full suite put 3862 and 5496 there. The
scenario now asserts the pair's wakeups, which is the workload's property, and
this is filed because the tool's attribution is wrong, not the test's count.

Wanted, in `src/bpf/wakeup.rs`: a wakeup in interrupt context does not name
the interrupted task as its waker -- read `preempt_count` (the per-cpu
`__preempt_count` on x86, through `bpf_this_cpu_ptr` of its ksym) and, under
`HARDIRQ_MASK | SOFTIRQ_MASK`, record the waker as the interrupt (tid 0 with a
flag) and keep the event only when its wakee is the target's. The `wakeup` join
already has a vocabulary for a chain that starts in the kernel.

---

## Needs from another stream — the eleventh wave, stream P (the portability matrix)

Stream P's deliverable is `docs/portability.md`. The matrix -- four kernels
(5.15, 6.1, 6.6, 7.2) under five configurations, and the host -- found ten
classes of source that `omn list` calls usable and that refuse when run. Every
refusal names its reason; what is wrong is that the probe or the resolver said
yes first, and for a launched target the program was already started. Each is
recorded in `KNOWN_DEFECTS` in `tests/portability.rs`, which fails when an
entry stops matching: whoever fixes one removes its entry there (or asks the
coordinator to). `scripts/matrix.sh check` re-judges the cells on disk without
booting anything; `scripts/matrix.sh v6.6.157 --configs caps` re-measures one.

### To whoever owns `src/bpf/mod.rs` (nobody this wave): P1, check the prototype in the probe

`raw_tracepoint_support(name)` answers whether `btf_trace_<name>` exists. The
programs then compare `Btf::tracepoint_signature(name)` with the prototype they
were written for and refuse at attach -- `proc.exit` and `mem.reclaim` on 6.6
and below, `io.latency`, `mem.kmalloc`, `mem.compaction` and `net.request`'s
`sched_switch` on 5.15. The check exists; the probe does not call it. Wanted,
with the expected prototype as data rather than code:

```rust
// A requirement may carry the prototype its program is written for:
//   raw_tracepoint:sched_process_exit(task_struct *, bool)
// and the probe compares it with the kernel's.
pub fn raw_tracepoint_support(requirement: &str) -> Support; // `name` or `name(arg, ...)`
```

and the catalog entries of the programs that check a signature at attach
(`src/bpf/{io,io_writeback,net_program,runqlat,mem_program,migrate,sched_lock,request}.rs`)
writing that prototype into their `raw_tracepoint:` requirement. On 5.15 this
is also what makes the floor hold for sources whose tracepoints exist below it.

### To whoever owns `catalog/net.yaml` and `src/capabilities.rs`: P2, a field the program reads

`net.drops`, `net.tcp.lifetime` and `net.tcp.retransmit` read `sock.sk_ino`,
which 6.6 does not have (*"this kernel's `sock` has no member named
`sk_ino`"*). A requirement class answered from BTF, and the three `bpf:raw_tp`
backends requiring it:

```rust
// in Capabilities::probe
if let Some(field) = requirement.strip_prefix("btf_member:") { // "sock.sk_ino"
    return Probe::from(crate::bpf::btf_member_support(field));
}
pub fn btf_member_support(struct_dot_member: &str) -> Support; // src/bpf/mod.rs
```

(or a fallback that reads the inode through `sk_socket->file->f_inode`, which
is `src/bpf/net_*.rs`'s choice).

### To whoever owns `src/capabilities.rs` and `src/bpf/mod.rs`: P3, the `uprobe` requirement below 6.6

Under `cap_bpf,cap_perfmon` on 5.15 and 6.1, every USDT source is listed usable
and refuses: the uprobe-multi link is refused, the fallback opens the `uprobe`
PMU, and that needs `CAP_SYS_ADMIN`. `uprobe` is answered by the PMU's
existence. Wanted: `uprobe` available only when a uprobe-multi link can be made
(Linux 6.6) or the process has `CAP_SYS_ADMIN`, and otherwise missing with
*"this kernel has no uprobe-multi link (Linux 6.6), and the uprobe PMU it would
fall back to needs CAP_SYS_ADMIN"*. And `permission_advice` should not be the
refusal's first sentence there: it tells a binary carrying `CAP_BPF` and
`CAP_PERFMON` to be granted them.

### To whoever owns `catalog/runtime.yaml`: P4, the USDT backends need BTF

`bpf:usdt` requires `uprobe` and `bpf`; `UsdtProgram::load` starts with
`kernel_btf()?`. With `/sys/kernel/btf/vmlinux` absent, `runtime.gc`,
`runtime.alloc` and `runtime.custom` are listed usable and refuse with *"cannot
read /sys/kernel/btf/vmlinux"*. Add `btf` to their `bpf:usdt` (and
`bpf:uprobe`) requirements, as `net.request`'s already has.

### To whoever owns `src/bpf/request.rs` and `catalog/net.yaml`: P5, `net.request` below 6.6

`net.request` links every site with `BPF_TRACE_UPROBE_MULTI` and has no
fallback, so on 6.1 -- as root too -- it fails with *"the kernel refused to link
the program to 0x387e: Invalid argument"*. Either the same fallback
`src/bpf/usdt.rs` has, or a requirement that says so (`kernel>=6.6` on its two
backends, with the reason in a caveat) and a refusal that names 6.6 rather than
`EINVAL`.

### To whoever owns `catalog/gpu.yaml` or `src/collect/gpu/mod.rs`: P6, a library without its driver

The guests see the host's `libnvidia-ml.so` and no GPU: every `vendor:nvml`
source is listed usable and refuses with *"nvmlInit failed: Driver Not
Loaded"*. `nvml_probe` answers "the library loads". A container with the
userspace library, or a host after a driver upgrade, is the same state. Either
the probe calls `nvmlInit_v2` (and `nvmlShutdown`) and reports NVML's own error,
or -- data only -- the `vendor:nvml` backends also require
`/proc/driver/nvidia/version`, which exists exactly when the kernel driver is
loaded.

### To whoever owns `src/resolve.rs` and `src/collect/mod.rs` (stream S this wave): P7, a collector that refuses the mode

The resolver walks past an unimplemented backend to a fallback whose collector
does not serve the requested mode, which then refuses at start, in every cell:
`cpu.oncpu_time -m trace` onto `fs:proc`, `power.cstate -m trace` onto
`fs:sys`, `mem.faults -m count` onto `pmu:sw` (your item 4), `sched.switches
-m count` onto `pmu:sw`. Wanted: a factory declares what it serves, and the
resolver treats a backend whose factory does not serve the request's mode as
`unimplemented` for that request, with the reason in the chain:

```rust
Factory::new("power.cstate", "fs:sys", ...).modes(&[ModeKind::Poll])
```

### To whoever owns `src/collect/cpu/profile.rs`, `src/collect/mem/page_faults.rs` (stream S) and `src/collect/sched/` (stream K): P8, `system: native` refused

The three catalogs declare `system: native`; the three collectors need a pid
and refuse *"profiling a whole machine needs one sampling event per cpu and the
privileges for them"* -- on a binary that has `CAP_PERFMON`. One event per cpu
(`perf_event_open(pid = -1, cpu = n)`), or `system` taken out of the three
entries' `targets`. The same `Factory` declaration as P7 (`.targets(...)`)
would let the resolver refuse it before the launch.

### To whoever owns `src/resolve.rs` (stream S): P9, a per-task probe for a per-cpu event

Without capabilities and with `perf_event_paranoid` at 2, a process may count
itself and not a cpu. The `pmu:` probe opens a per-task event, so ten counting
sources are listed usable and `--system` refuses (correctly worded) at start.
The source is usable, the request is not: the resolver has the target, and a
`pmu:` requirement probed with `cpu = 0, pid = -1` for a system target would
refuse it with the reason before anything runs.

### To whoever owns `catalog/power.yaml` and `catalog/mem.yaml`: P10, two requirements that name the wrong thing

- `power.freq`'s `fs:sys` requires `/sys/devices/system/cpu/cpufreq`, which
  exists in a guest with no cpufreq driver; the collector reads
  `/sys/devices/system/cpu/cpu0/cpufreq`. Require that path.
- `mem.swap`'s `fs:cgroup` reads `pswpin`/`pswpout` from `memory.stat`, which
  6.6 does not publish and 7.2 does; the entry requires nothing that says so.

(`mem.cgroup` refusing the root cgroup is exact and is the guest's shape, not a
defect; it is recorded for the guest cells only.)

### To the owners of five tests: they fail without capabilities, and the CI runs without them

Rehearsed here with the binary stripped of its capabilities (both hard links:
`target/debug/omn` and cargo's `deps/omn-*`), the suite gives 1772 passed and
these five failed -- each asserts what a privileged machine answers instead of
skipping, or accepting the unprivileged answer, where the capability is
missing:

| Test | File | Owner this wave |
|---|---|---|
| `a_program_that_declares_no_request_is_refused_by_name` | `tests/vertical.rs` | J |
| `every_mode_and_emit_of_the_kernel_sources_is_served_or_refused_by_the_right_code` | `tests/scenario_mem.rs` | S |
| `the_combinations_these_sources_do_not_have_are_refused_with_the_reason` | `tests/scenario_proc.rs` | S |
| `the_syscall_filter_the_resolved_file_claims_is_the_one_that_ran` | `tests/scenario_proc.rs` | S |
| `what_this_machine_cannot_check_about_gpu_job_is_named_with_its_numbers` | `tests/scenario_gpu.rs` | nobody |

And one skip that gives no reason: `tests/vertical.rs` prints
`stderr.lines().next()` as its skip reason (lines 921, 997, 1104), which on
this machine is `note: mode defaulted to trace, the only one net.request
supports`. Printing the first line that starts with `error:` -- or the whole
refusal -- says why. `.github/scripts/skip-gate.sh` fails the CI on it, as it
should.

### For the flaky tally: two seen under the eleventh wave's load, passing alone

- `every_mode_and_emit_of_the_sampled_sources_is_served_or_refused_by_the_right_code`
  (`tests/scenario_mem.rs`): `mem.latency sample stack` joined the exit-1
  refusals in two full runs on stream P's branch, which touches no `src/`
  file; passed alone.
- `the_page_cache_sessions_say_what_their_headers_predict`
  (`tests/scenario_io.rs`): once in a full run; passed alone.

### To the coordinator: `make matrix`, and when to push the workflow

The recipe is in `docs/portability.md`, "Running it". `.github/workflows/ci.yml`
has never run: the first push will show the runner's kernel, whether its skips
all match `.github/expected-skips.txt`, and whether its cell meets a defect
`KNOWN_DEFECTS` does not record for a `ci-*` cell. Each is a one-line change.

## Needs from another stream — the twelfth wave, stream M (a model reads it)

`omn-mcp` (`mcp/`, `docs/mcp.md`) probes nothing itself: a probe answers for
the process that asks, and the same library linked into the server, without
`CAP_BPF` and `CAP_PERFMON` and without the shim beside it, answers a sweep
forty-two sources worse on the development machine (52 usable against 93) --
silently. So it asks the
`omn` binary. Two of the things it asks come back only as text or only as a
private function, and each is worked around under a check that makes a drift
loud; neither blocks the stream.

### To stream R, for `src/main.rs`: `omn list --format json` carries the sweep

The JSON listing is the catalog without the `HERE` column, so the one
machine-readable projection of the capability sweep does not exist.
`omn-mcp` reads the text table meanwhile, strictly (header columns, every row
a catalog id in catalog order, `yes`/`?`/`no` only, tallies equal to the
footer; anything else refused), in `mcp/src/binary.rs::parse_sweep`. Wanted,
in the JSON branch of `list`:

```rust
#[derive(Serialize)]
struct Listed<'a> {
    #[serde(flatten)]
    entry: &'a Entry,
    /// `resolve::availability(entry, &capabilities)`, which already derives
    /// `Serialize` as {"availability": "ready"|"unverified"|"unavailable", "backend": id}.
    here: resolve::Availability,
    /// Per backend, in chain order: `resolve::probe_all(capabilities, backend, None)`.
    probes: Vec<(String, Vec<resolve::Requirement>)>,
}
```

(`probe_all` is private to `src/resolve.rs`, which is V's: it would need `pub`.)
Once it lands, `parse_sweep` is deleted and the test
`the_sweep_is_the_one_the_binary_that_runs_gives` keeps checking the totals.

### To stream V, for `src/catalog.rs`: the worst case of a cost band, in one place

`worst_case_percent(Cost) -> f64` is private to `src/main.rs`, and
`plan_session` states the band's worst case beside the backend it would
select, so `mcp/src/session.rs::band` holds a second copy of the four numbers.
Wanted: `impl Cost { pub fn worst_case_percent(self) -> f64 }` in
`src/catalog.rs`, which `main.rs` (R) and `omn-mcp` then both call.

### To nobody (unowned `src/join.rs`): what an evidence level means, in one place

`mcp/src/evidence.rs` says beside every `evidence` field what the level
establishes and what it does not -- the reason `IDENTITY` and `TEMPORAL` do
not read alike to a model. The sentences come from `join.rs`'s doc comments,
and the match on `Evidence` is exhaustive, so a new level does not compile in
`omn-mcp` until it is described. Wanted when someone owns `join.rs`:
`impl Evidence { pub fn establishes(self) -> &'static str; pub fn does_not_establish(self) -> &'static str }`,
which the text renderer could print too.
