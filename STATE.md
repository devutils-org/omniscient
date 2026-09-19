# State of the project

Where Omniscient actually is, measured rather than remembered. Every number here
was read off the tool or the suite on the development machine, and every claim
about what is missing names what it is blocked on.

Read `ROADMAP.md` for the plan, `DECISIONS.md` for what is settled,
`docs/open-findings.md` for the findings that cut across families, and this file
for where the two meet.

---

## In one paragraph

The request model, the catalog, the resolver, the validator, the collector
layer, the emit and encode layers and a join engine all exist and are used by
every source. Forty-four sources across nine families collect for real, over six
mechanisms: procfs and sysfs, `perf_event_open` counting and sampling, netlink,
an `LD_PRELOAD` shim, NVML through `dlopen`, and eBPF raw tracepoints. Every
emit the catalog declares exists — `raw`, `timeline`, `summary`, `rate`,
`hist`, `topk` and `stack` — and stacks are captured, symbolized and written as
`folded` and `speedscope`. The first correlation works: page faults attributed
to the allocations whose ranges contain them, with `KEY` evidence. **633 tests
pass**, and the suite is the project's main asset — several of its tests have
found real defects in the code they were written against.

What is missing is breadth, not skeleton. Adding a source is additive; the
expensive parts are done.

---

## What the numbers say

```
100 sources: 80 usable here, 15 unverified, 5 out of reach on this machine.
44 of them have a collector, covering 48 of their 165 backends.
```

Three different questions, deliberately kept apart.

| Question | Answer | Meaning |
|---|---|---|
| What can this machine serve? | 80 | probed now: the file is readable, the event opens, the tracepoint is there |
| What can this build collect? | 44 sources, 48 backends | a collector is written and registered |
| What is neither? | the rest | vocabulary: it resolves, it validates, and it says so |

That `80` is with `CAP_BPF`, `CAP_PERFMON` and readable tracefs. The same
binary without them reports `53`: the difference is the whole eBPF and per-cpu
perf surface, and it is why `make` reapplies the capabilities that every relink
drops. Nothing announces the loss — the tool reports fewer sources, which reads
like the machine changed rather than the binary.

It cuts the other way too, which cost a session to find: a binary that carries
capabilities is non-dumpable, so its own `/proc` entries belong to root and the
three `mem.heap.*` sources lost the `pagemap` they read. Finding 14 of
`docs/open-findings.md` has it; `main.rs` now restores dumpability at startup.

Per family, sources with a collector:

| Family | With a collector | Sources | Usable here |
|---|---|---|---|
| `cpu` | 11 | 14 | 12 |
| `mem` | 7 | 23 | 20 |
| `gpu` | 5 | 9 | 6 |
| `io` | 5 | 14 | 11 |
| `net` | 5 | 15 | 10 |
| `power` | 5 | 5 | 4 |
| `sched` | 3 | 10 | 9 |
| `proc` | 2 | 6 | 6 |
| `runtime` | 1 | 4 | 1 |

---

## What works end to end

Each of these has a ground-truth test: a check against something independently
known, not against what the code computed.

| Source | Mechanism | How it is checked |
|---|---|---|
| `cpu.instructions`, `.cycles`, `.ipc`, and the rest of the counters | `perf_event_open`, counting | a hundred million known instructions, user-mode only, within 1 % and with a run-to-run spread under 1 % |
| `cpu.util`, `.pressure`, `.throttle` | procfs, cgroup | against `/proc/stat`'s own accounting |
| `mem.rss`, `.pss`, `.uss` | procfs | two backends cross-checked against each other |
| `mem.faults` | `perf_event_open`, sampling | against the kernel's own minor-fault counter for the same process |
| `mem.heap.alloc`, `.live`, `.retained` | `LD_PRELOAD` shim | allocation sizes the test chose |
| `io.bytes`, `.iops`, `.inflight`, `.merges`, `.pressure` | `/proc/diskstats`, PSI | a device resolved through `mountinfo`, with known I/O pushed through it |
| `net.bytes`, `.packets` | `/proc/net/dev` | 16 MiB pushed through loopback |
| `net.tcp.rtt` | netlink `sock_diag` | a loopback connection found in the dump, keyed by its cookie |
| `net.iface`, `.backlog` | procfs | a one-deep accept queue given 64 connections |
| `power.energy` | RAPL through perf, and sysfs | about 40 W package draw, scale read from the kernel |
| `power.freq`, `.cstate`, `.temp`, `.throttle` | sysfs, MSR | `/proc/stat`'s independent idle accounting |
| `gpu.freq`, `.power`, `.temp`, `.mem`, `.engine_busy` | NVML through `dlopen` | 256 MiB of VRAM allocated and read back to the byte |
| `proc.fd`, `.threads` | procfs | a launched process that reports its own counts; asserted equal, not close |
| `runtime.jit` | `/tmp/perf-<pid>.map` | a map the test wrote |
| `sched.wakeup` | eBPF raw tracepoint | the causal chain of a shell and its children, loaded and attached for real |
| `sched.switches` | `perf_event_open` | against the kernel's own switch counter for the same thread |
| `sched.offcpu` | eBPF raw tracepoint | a thread that sleeps a known 300 ms, whose off-cpu duration and interruptible state must both match |
| `cpu.profile` | `perf_event_open`, sampling with `PERF_SAMPLE_CALLCHAIN` | a cpu-bound loop in the test binary, whose own symbols must appear in the table |

The correlation, from `examples/allocation-cost.yml`:

```
join pages-of-allocation  mem.heap.alloc -> mem.faults  evidence KEY
  2655 of 3518 `mem.faults` events attributed (75.5 %); 863 fell outside every open range
  states: 2555 opened, 2524 closed, 31 still open at the end, peak 1579 held at once
  no eviction: nothing was dropped for age or for space
```

---

## Where each roadmap phase stands

| Phase | State |
|---|---|
| 0 — decisions | **done**. Language, clock, scope, kernel floor 6.1, `CAP_BPF`+`CAP_PERFMON`, raw tracepoints first. |
| 1 — event model and request schema | **done**. `Event`, `Span`-equivalent attributions, `Request`, `ResolvedRequest`, typed keys, one clock domain. |
| 2 — catalog, resolver, CLI | **done**. 100 entries as data, machine introspection, a resolver that records why each backend was passed over, a validator that refuses before attaching. |
| 3 — first collectors and transport | **done**. Ring buffers for perf and eBPF, loss accounting including throttling, a launch gate, frame-pointer unwinding and symbolization through the target's own maps. |
| 4 — the vertical case | **partly**. One real correlation exists and is checked. The HTTP-request decomposition it describes needs the eBPF backends of `io` and `net`, and the `wakeup` match kind. |
| 5 — generalizing the join engine | **started**. Rules are declarative and evaluated; only `interval` matches. `eq`, `scoped_eq` and `wakeup` are refused by name. |
| 6 — emit, aggregation, budget | **partly**. Every emit exists: `raw`, `timeline`, `summary`, `rate`, `hist`, `topk` and `stack`, with `folded` and `speedscope` encoders behind the last. `--budget` is still parsed and not enforced. |
| 7 — declarative configuration | **done**. Sessions in YAML, JSON and TOML; a dumped request reads back unchanged; the resolved file is written beside the results. |
| 8 — self-benchmark | **not started**. `--backend force:` exists for it. |
| 9 — breadth | in progress; see the table above. |
| 10 — intelligence layer | not started. |

---

## What remains, in the order it is worth doing

### 1. Stacks on the sources that declare them

`cpu.profile` is the only producer. The catalog declares `stack` on nineteen
sources, and the rest of them — `sched.offcpu` above all, where the stack *is*
the diagnosis of why a task blocked — need the collector to intern a callchain
the same way. The seam is `Collector::stacks()`, and on the eBPF side it means
`bpf_get_stackid` and a stack map rather than a perf callchain.

The one unwind mode that works is `fp`. `--stack dwarf` and `--stack lbr` are
refused by name with what they would need, which is the honest answer: frame
pointers where the target has none report leaf functions as though they were
stacks, and this build says so as a degradation when it detects it.

### 2. The `wakeup` match kind

The join engine implements `interval` only. `sched.wakeup` produces correct,
uncorrelated events today. This is the single highest-value thing left, because
attributing latency *across a block* is what no other family can do for itself,
and every cross-layer story in `ROADMAP.md` phase 4 depends on it.

One caveat found while building it and worth carrying: a waker can be the task
that was merely interrupted, since softirqs run in the interrupted task's
context, and a waker of pid 0 is the idle task. The join is only as good as the
waker.

### 3. Per-backend targets and filter fields

The model gap three threads hit independently in one session, written up as
finding 1 of `docs/open-findings.md`. `catalog::Backend` says nothing about
which targets it serves or which fields it carries, so `io.bytes --system`
resolves onto a cgroup backend, and 26 sources declare a filter field no
collector emits — which, now that the userspace filter runs, silently empties a
series.

It needs a decision before it needs code: give `Backend` its own `targets` and
`filter_fields` falling back to the entry's, or accept that an entry describes
the union and make the resolver say which backend drops what.

### 4. Keys for a device and a cpu

`KeyValue` has `Iface` and nothing for a block device or a cpu, and a series is
keyed on an event's first key. So the `io` diskstats sources sum every device
into one number, `power` cannot produce a per-cpu series, and the `topk` over
devices that `io.bytes` declares cannot be expressed at all. Additive, and
finding 2 of the same document.

### 5. eBPF beyond raw tracepoints

Only raw tracepoints attach. `io`, `net` and `runtime` need kprobes, uprobes,
fentry and USDT; `sched.offcpu` needs in-kernel aggregation, which is also what
makes a syscall tracer affordable. `docs/bpf-layer.md` has the calling
conventions.

### 6. The smaller ones, each with its reason

- **A throttled perf event is a loss of unknown size.** `sched.switches` counts
  it; `mem.faults` ignores it. `Losses` should carry a `throttled` count of its
  own rather than folding it into `dropped`, which overstates precision.
- **The closing read of a polled counter.** `net` takes one and `io`/`power`
  deliberately do not, for a reason each. Both cannot be right, and a reader
  comparing two families gets a different convention in each.
- **An exit code for "valid, but this mechanism is not built".** It returns 1,
  like a real failure; `NoCollector` returns 3 and this belongs with it.
- **`--budget` is parsed and ignored.** Enforcing it means measuring the
  collector's own cost and degrading `trace` to an in-kernel `hist`, which needs
  item 1 first.
- **`Unit::Watts` does not exist**, which is part of why `gpu.power` reports an
  energy counter.
- **The GPU is not in the environment capture**, and two runs are comparable
  only if they were taken on the same device under the same driver.

---

## What to be careful of

Things that have already cost time, and will again.

**Capabilities do not survive a relink.** Every `cargo build` drops them, and
nothing announces it: the tool reports fewer sources as available, which reads
like the machine changed. `make` reapplies them.

**A test that skips is not a test that passes.** Three threads lost time to
this in one session, in three different shapes. A machine that cannot serve a
request and a request that cannot mean anything are different answers, and only
the first is a reason to stop quietly.

**A green suite on an unprivileged machine says nothing about the eBPF path.**
Those tests skip, loudly. The integration tests do take the privileged path once
`target/debug/omn` carries the capabilities; the in-process unit tests cannot.

**Measure before explaining.** Every surprising number in this project so far
has turned out to be right, and the expectation wrong: a fault is not a page,
memory the allocator already holds faults nothing, an idle share above 1.0 for
one tick is how the kernel credits residency, and a counter that counts kernel
mode is not comparable to a user-mode expectation.

**`cargo test` captures the print macros, not file descriptor one.** A suite
launched from a terminal has a terminal on stdout inside its own tests. Anything
that asks the process about its own environment is testing how the suite was
started.
