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

## 11. Three flaky ground-truth tests, and what each one is really measuring

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
