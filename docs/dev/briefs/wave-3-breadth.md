# Brief — breadth, and the mechanism that unlocks most of it

Six work streams, meant to run as independent agents on separate branches. This
is the third fan-out on this project, and it is divided a third way, for a
reason worth understanding before dispatching anyone.

The **first** wave split by family: nine agents, nine directories, merges that
could not conflict. It landed 42 of 100 sources.

The **second** split by shared layer — emit, stacks, joins, the catalog model,
eBPF, and the small honest ones. It landed everything those sources needed to be
expressible: all seven emits, all four join match kinds, stack capture through
two different mechanisms, and per-backend targets and filter fields.

What is left is **breadth again**, but breadth is no longer evenly distributed.
Fifty-six sources have no collector and **twenty-seven of them wait on one
piece of work**: `src/bpf/` attaches exactly one way. So this wave is one stream
that builds mechanism, four that build sources needing no new mechanism, and one
that finishes the tool's own honesty about what it costs.

Read `docs/dev/STATE.md` for where the project is, `docs/contributing.md` for what ground
truth means, and `CLAUDE.md` for the invariants. They apply to every stream.

---

## The streams

| | Stream | Owns | Sources it lands | Unlocks |
|---|---|---|---|---|
| **A** | eBPF attach kinds | `src/bpf/**`, `src/collect/sched/**` | 5 | **20 more**, and roadmap phase 4 |
| **B** | Memory breadth | `src/collect/mem/**` | 6 | — |
| **C** | Hardware counters and per-cpu perf | `src/perf.rs`, `src/collect/cpu/**` | 5 | per-cpu series for every family |
| **D** | Profile and trace encoders | `src/encode.rs`, `src/format.rs` | — | pprof, Parca, Perfetto |
| **E** | Cost, budget and the self-benchmark | `src/collect/mod.rs`, `src/main.rs` | — | roadmap phase 8 |
| **F** | Network breadth | `src/collect/net/**` | 2 | packet-level evidence |

Nothing here blocks anything else *in this wave*. A is first among equals
because what it builds is what the next wave needs, not because anyone is
waiting on it now.

### Two seams, named in advance

**The registry line.** Each family registers its collectors in its own
`src/collect/<family>/mod.rs`, in a list of `Factory::new` calls. A stream that
adds a source appends to its own family's list, so the streams do not meet —
with one exception, below. The shape was chosen for exactly this: nine agents
editing one central `match` arm is nine conflicts on the same three lines.

**The counters table.** `src/collect/cpu/counters.rs` holds `SPECS`, one row per
`perf_event_open` counting source, and adding a counter is adding a row. It is a
*mechanism* module that happens to live under `cpu/`, and three of the sources
this wave adds through it — `mem.cache_l1`, `mem.cache_llc`, `mem.cache_tlb` —
belong to the `mem` family by name. **Stream C owns them**, not B. The mechanism
decides who writes a source, not the first word of its id; a row in a table is
not a reason to grow a second table beside it. B is told the same thing in its
section, so neither discovers it in a merge.

---

## Rules

A stream may create and edit **only** the paths listed as its own. Everything
else is read-only to it. Needing something from another stream's files is not a
reason to edit them: write the request under a `## Needs from another stream`
heading in `docs/open-findings.md`, with the exact signature wanted and why, and
carry on with what is unblocked.

`catalog/*.yaml` is the one file every stream touches, one per family, and only
to add a backend or a caveat to its own sources. Those are appends within a
source's own block; they do not collide.

One branch per stream, `feat/<stream>`, in its own worktree — `scripts/spawn.sh
<stream>` makes both and prints the prompt. If a merge conflicts anywhere other
than a registry line, a stream edited something it does not own; that is the
bug, not the merge.

A stream finishes by writing `LOG_<STREAM>.md` at the repository root: what it
landed, what it measured, what it found and left alone, and what the next person
needs to know that is not in the code. The coordinator reads those, not the
diffs.

A stream is done when `make check` is clean, the behaviour it adds has a test
that would fail without it, and anything it discovered that belongs to someone
else is written down rather than fixed in passing.

---

## Stream A — eBPF attach kinds

> **Mission.** `src/bpf/` attaches exactly one way, and that is why twenty-seven
> sources have no collector. Build the other ways, and prove each on a source
> that needs it. You own `src/bpf/**` and `src/collect/sched/**`.

**What exists.** `BPF_RAW_TRACEPOINT_OPEN`, a BTF reader, a hand-assembled
instruction builder with an auditor, hash maps, per-cpu counters, a ring buffer,
and a stack-trace map. `sched.wakeup` and `sched.offcpu` are the two working
customers, and `offcpu.rs` is the worked example of in-kernel aggregation with
`bpf_get_stackid`.

**What to build, in the order the catalog asks for it.**

| Attach | Asked by | What it needs |
|---|---|---|
| `bpf:tp` | 27 sources | the numeric id under `tracefs`, `perf_event_open` with `PERF_TYPE_TRACEPOINT`, then `PERF_EVENT_IOC_SET_BPF` |
| `bpf:fentry` | 5 | `BPF_PROG_TYPE_TRACING`, `attach_btf_id` from the kernel's BTF, `BPF_RAW_TRACEPOINT_OPEN` with a target |
| `bpf:kprobe` | fallbacks | `perf_event_open` with `PERF_TYPE_KPROBE` and the symbol in `config1` |

**Prove them on `sched`**, which you own: `sched.runqlat` (`bpf:tp`, the
`sched_wakeup` → `sched_switch` pair, and the one source whose whole value is
the difference between two events), `sched.migrations`, `sched.lock`,
`sched.irq`, `sched.futex`.

**The cheaper half of your own mission.** Several `bpf:tp` entries name events
`raw_tp` can attach to just as well — same events, strictly fewer requirements,
which is the ordering `docs/DECISIONS.md` settled. Auditing the catalog for those and
moving them is a catalog change with an existing mechanism behind it. Do that
first: it is sources landed for a fraction of the work, and it tells you which
entries genuinely need the classic path.

**Traps.**

- `tracefs` is root-only on a stock machine, and the whole reason `raw_tp` is
  ordered first. A `bpf:tp` backend that needs a file this user cannot read must
  say so as a missing requirement, not fail at attach time.
- `bpf_ktime_get_boot_ns` includes suspend. `CLOCK_MONOTONIC` is the only clock
  domain in this tool, and there is a test in `offcpu.rs` that checks the
  instruction stream itself for this. Keep that pattern.
- Streaming one event per tracepoint hit is a measurement whose cost grows with
  the workload. `sched.runqlat` is a histogram: build it in the kernel.

**Ground truth.** A run queue latency test can inject one: pin two busy threads
to one cpu and the wait is real and bounded. `sched.lock` can be checked against
a program that contends a mutex a known number of times. A test that only checks
the program loads has not measured anything.

---

## Stream B — memory breadth

> **Mission.** Six memory sources that need no privileges at all and have been
> left behind because eBPF is more interesting. You own `src/collect/mem/**`.

**What to build.**

| Source | Backend | What it is |
|---|---|---|
| `mem.pressure` | `fs:proc`, `fs:cgroup` | PSI: `some` and `full`, and they mean different things |
| `mem.swap` | `fs:proc`, `fs:cgroup` | what is paged out, which is not what is free |
| `mem.thp` | `fs:sys`, `fs:proc` | huge page allocation and collapse |
| `mem.cgroup` | `fs:cgroup` | the accounting a container is actually killed by |
| `mem.numa` | `fs:proc` | per-node residency, the fallback the `pmu` backend degrades to |
| `mem.wss` | `fs:sys:page_idle` | working set, by clearing and re-reading the idle bitmap |

**Not yours**: `mem.cache_l1`, `mem.cache_llc` and `mem.cache_tlb` are `pmu`
counters and belong to stream C, which owns the counters table. `mem.reclaim`
and `mem.compaction` are `bpf:tp` and wait on stream A.

**Traps.**

- **PSI `some` and `full` are not two precisions of one number.** `some` is at
  least one task stalled, `full` is every runnable task stalled — the second is
  the one that means the machine has stopped. `io.pressure` already carries both
  and says so; read it before writing this.
- **`mem.wss` changes what it measures.** Clearing the idle bitmap makes the
  next read meaningful and costs the kernel a walk of every page; it is also
  visible to anything else reading it. Say so as a caveat, and price it in the
  backend's declared cost.
- **A cgroup path is not a target.** `mem.cgroup --pid` has to resolve the pid's
  cgroup, and a process in the root cgroup is not an error.
- Absent is not zero. A kernel without `/proc/pressure` and a machine under no
  pressure are different answers, and only the second is a number.

**Ground truth.** Allocate and touch a known number of bytes and check the
per-node residency adds up. Force swap with a cgroup memory limit and a known
allocation, and check what `mem.swap` reports against `/proc/<pid>/smaps_rollup`.
`mem.thp` can be checked against `/sys/kernel/mm/transparent_hugepage` and a
2 MiB-aligned mapping that is touched.

---

## Stream C — hardware counters and per-cpu perf

> **Mission.** `src/perf.rs` has `Scope::Cpu` and no collector uses it, so no
> source in the tool can produce a per-cpu series through the PMU. Fix that, and
> land the five counting sources that have been waiting on it. You own
> `src/perf.rs` and `src/collect/cpu/**`.

**What to build.**

- **Per-cpu counting.** One event per online cpu, summed or kept apart as the
  request asks. This is what makes a `--system` or `--cpu` target mean anything
  for the PMU, and today those requests are refused with "counting a cpu means
  one event per cpu, which is a different thing to build".
- **`cpu.topdown`** — the Intel/AMD top-down methodology: what fraction of issue
  slots went to retiring, bad speculation, frontend and backend bound. It is a
  *derived* source over several counters read in one group, and the derivation
  is the value: four numbers that sum to one, not four raw counts.
- **`cpu.oncpu_time`** through `fs:proc` — cpu time actually consumed, which is
  the denominator half the other sources need and the cheapest source left.
- **`mem.cache_l1`, `mem.cache_llc`, `mem.cache_tlb`** — rows in
  `counters::SPECS`, using `PERF_TYPE_HW_CACHE` with its three-part config. They
  are named `mem.*` and they are yours: the mechanism decides who writes a
  source, and a row in a table is not a reason to grow a second table.

**Traps.**

- **Per-cpu events need privileges most machines do not grant**, and the
  refusal has to name which — `perf_event_paranoid`, or `CAP_PERFMON`. A
  backend that silently falls back to a per-task number is answering a different
  question under the same name.
- **A counter group that does not fit in hardware is time-shared**, and the
  kernel reports `enabled_ns` and `running_ns` so the caller can scale. The
  existing counters already carry both; top-down reads several counters at once
  and is exactly where an unscaled ratio becomes a wrong number rather than a
  noisy one.
- **`PERF_TYPE_HW_CACHE` config is three fields packed into one**: cache id,
  operation, result. Getting the packing wrong gives a counter that opens, runs,
  and counts something else.
- A cpu that goes offline mid-run is a read that failed, counted and explained;
  `power/freq.rs` has the pattern.

**Ground truth.** A program that walks an array larger than the last-level cache
misses on nearly every access, and one that walks a 4 KiB array misses on almost
none — the ratio between the two is the check, not the absolute count. Top-down
fractions must sum to 1.0 within the counter's own error, on any workload.

---

## Stream D — profile and trace encoders

> **Mission.** Two output formats are declared, refuse by name, and are now
> unblocked. You own `src/encode.rs` and `src/format.rs`.

**What exists.** `text`, `json`, `yaml`, `toml`, `ndjson`, `folded` and
`speedscope`. `folded` and `speedscope` both carry stacks and both work, so the
stack side has a producer and a shape to follow: `cpu.profile --emit stack` and
`sched.offcpu --emit stack`.

**What to build.**

- **`pprof`** — gzipped `profile.proto`. The whole format is a string table and
  indices into it, which is the same interning `Results::stacks` already does;
  the work is the protobuf encoding and the sample-type header, not the
  aggregation. Parca and `go tool pprof` read it, and it is the format a
  continuous-profiling pipeline expects.
- **`chrome`** — Chrome Trace Event Format, which Perfetto reads. It carries
  *timelines*, not stacks, so it is the first format for which `--emit timeline`
  is the natural input and `--emit stack` should be refused.

**Traps.**

- `format.rs` declares what each format can carry and whether its encoder
  exists. There is a test that crosses the claim with reality, and it exists
  because a merge once reverted an encoder and left the claim behind. Keep it
  true.
- A format that cannot express an emit must refuse it by name rather than
  approximate. `folded` already does this; the refusal is the feature.
- `pprof`'s sample types have units, and this tool's `Unit` does not map onto
  them one to one. Where it does not, say so rather than picking the nearest.

**Ground truth.** Round-trip: encode a known profile and decode it with the real
tool — `protoc --decode` against the upstream `profile.proto` for pprof, and
Perfetto's own validator or a `json.loads` plus a schema check for chrome. A
test that checks the encoder produced what the encoder computed proves nothing.

---

## Stream E — cost, budget and the self-benchmark

> **Mission.** `--budget` is parsed, explained and not enforced, and the tool
> cannot say what it costs. Both halves are one piece of work. You own
> `src/collect/mod.rs` and `src/main.rs`.

**What exists.** `--budget 2%` is validated, and the run prints which backend it
would have chosen and whether the selected one is within the ceiling — from the
catalog's *declared* cost band, which is a static claim nobody has checked.

**What to build.**

- **Measure the collector's own overhead.** The runner knows when each tick
  started and how long it took; what is missing is the comparison against the
  observed program's own cost. `getrusage(RUSAGE_SELF)` over the run, against
  the wall time and the target's own cpu time, is the cheapest honest answer.
- **Enforce the budget.** A run that exceeds its ceiling degrades or stops, and
  either way says which. Degrading means moving to a cheaper backend mid-run,
  which this architecture can express and has never done.
- **The self-benchmark.** `--backend force:<id>` exists for exactly this and
  nothing uses it: run the same measurement through each backend of a chain and
  report what each cost and whether they agreed. That is roadmap phase 8, and it
  is what turns the catalog's cost bands from a claim into a measurement.

**Traps.**

- **Measuring your own cost changes it.** Say what the measurement costs, and
  do not sample it so often that the answer is mostly itself.
- **A cost band is per backend, not per source.** The same source through
  `fs:proc` and through `bpf:tp` are different measurements at different prices,
  which is the whole reason the band lives on the backend.
- The registry in `src/collect/mod.rs` is the seam every family appends to. You
  own the file; leave the registry list alone beyond what you need.

**Ground truth.** A collector polling at 10 Hz and one draining a ring under a
million switches a second must come out on different sides of a 2 % budget, and
the tool has to agree with `time` about what it spent.

---

## Stream F — network breadth

> **Mission.** Two network sources that need no eBPF, and one of them is a
> mechanism this tool does not have. You own `src/collect/net/**`.

**What to build.**

- **`net.capture`** through `af_packet` — a raw socket with a classic BPF
  filter, `PACKET_MMAP` for the ring, and `SO_TIMESTAMPNS` for timestamps in the
  one clock domain. The probe for `af_packet` already exists in
  `src/capabilities.rs` and answers on this machine; nothing uses it.
- **`net.hwts`** through `netlink` — hardware timestamping capability and state,
  which is what says whether a latency measured here is a NIC's word or the
  kernel's guess. `net.tcp.rtt` already does `sock_diag` over netlink; the
  socket and the parsing pattern are there to follow.

**Traps.**

- **A packet capture is a privacy boundary.** What this tool writes out must be
  what the request asked for and no more: a length, a five-tuple, a timestamp —
  never payload unless the request said so, and then it must say so in the
  output too.
- **`SO_TIMESTAMPNS` is not `CLOCK_MONOTONIC`.** It is realtime by default, and
  a timestamp from another clock domain carries a non-zero `ts_err_ns` in this
  tool — or is converted, and says it was. `SOF_TIMESTAMPING_*` is how to ask
  for the monotonic one; if the NIC will not give it, that is a degradation with
  a name.
- **A full ring is a loss, and af_packet reports it** in `tpacket_stats`.
  Reading it is not optional: a capture that dropped a third of its packets and
  says nothing is the failure this whole project is built against.
- `net.capture` needs `CAP_NET_RAW`, which the build does not currently grant.
  Say what is missing; do not widen the Makefile's capability set without
  writing down why in `docs/open-findings.md` — stream E owns that decision's
  neighbourhood and the coordinator owns the Makefile.

**Ground truth.** Send a known number of packets of a known size over loopback
and check the capture reports that many, of that size, within the window. The
same loopback traffic `net.bytes` already measures itself against.

---

## If fewer hands are available

**A, then B.** A is the only stream whose output is other streams' input: it
unlocks twenty more sources and the whole of roadmap phase 4's vertical case.
B is the cheapest breadth in the project — six sources, no privileges, no new
mechanism — and it is the one that keeps getting skipped.

**C and D are the safest to run unattended**: both own a small, closed set of
files that nothing else in this wave touches, and both have ground truth that
does not depend on the machine's privileges.
