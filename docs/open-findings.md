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