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

## 8. Thirty-six sources ask for tracefs where a raw tracepoint would do

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

## 9. The sudoers line the Makefile documents cannot match, twice over

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

## 10. Three flaky ground-truth tests, and what each one is really measuring

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

## 11. What stack capture measured

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