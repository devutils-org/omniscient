# Guide — running the demonstrations, and what each one shows

A hundred and forty-five sessions, nine families and their crossings, four kinds of question. This is how to
run them and what to look for.

Everything here runs against **this** machine, now. Nothing is recorded, nothing
is staged, and a session this machine cannot serve says so rather than printing
something plausible — which is half of what there is to see.

```console
make                    # build, and grant the capabilities a relink drops
make demo               # the guided tour: eight questions, about seven seconds
scripts/demo.sh --list  # every session, by family, without running any
scripts/demo.sh mem     # one family's sessions
make demo-all           # the tour and then all one hundred and forty-five
omn -f examples/mem-footprints.yml     # one session on its own
```

`make` rather than `cargo build`, for two reasons that both fail silently:
`--workspace` builds the preload shim that `mem.heap.*` loads, and the eBPF and
per-cpu perf backends need `CAP_BPF` and `CAP_PERFMON`, which **every relink
drops**. Without them this machine serves 51 sources instead of 86, and nothing
announces it — the tool simply reports fewer things as available, which reads
like the machine changed rather than the binary.

---

## Start here: the guided tour

`make demo` answers eight questions in order. It is the fastest way to see what
the tool is.

| | What it shows | Why it is in the tour |
|---|---|---|
| **1** | What this machine can serve | Three different questions kept apart: what the machine offers, what this build can measure, and what is neither. Most confusion about this tool is confusion between those three. |
| **2** | Measuring a program that is running | The program is held at a gate until the collectors are attached, so the window starts before its first instruction. |
| **3** | The number no single source can give | `malloc` allocates nothing; pages become resident on first write. An allocation profile and a memory curve never line up, and joining them by the range each allocation owns is what answers *which allocation cost memory*. |
| **4** | Talking to the kernel directly | Counters, netlink, RAPL and a perf ring — no `perf(1)`, no `bpftrace`, no second clock in the middle. |
| **5** | Who unblocked whom | The wakeup chain is the only way to attribute latency across a block without instrumenting the application. |
| **6** | What it refuses, and why | A refusal is a result. Each of these fails before anything is attached and says what the source does accept. |
| **7** | The record kept beside the numbers | Two runs through different backends are not comparable, and without this file nobody can tell that they were. |
| **8** | The same request, several ways out | A dumped request is a session file the tool reads back, so a measurement prototyped on the command line is frozen without being retyped. |

---

## The four kinds of session

Every family's sessions cover the same four, because each catches a different
kind of lie. Knowing which kind you are looking at is most of knowing what to
look for.

### Ground truth — *the workload chose the number*

The session measures a program whose property is written in its own source:
twenty thousand allocations of 4001 bytes, a hundred million instructions, two
hundred migrations, a gibibyte of reads. The tool has to find that number.

`mem-allocation-exact.yml`, `cpu-known-work.yml`, `sched-round-trips.yml`,
`gpu-vram-step.yml`, `io-known-volume.yml`, `net-injected-rtt.yml`.

**What to look for:** the measured number against the one in the session's own
comment header. These are the sessions that would catch a plausible lie.

### The capability sweep — *does every axis the catalog declares work?*

One session exercising every mode, emit and format a family offers. The catalog
is data and the command line is a projection of it, so a combination the catalog
promises and the build cannot serve is a promise broken where nobody looks.

`mem-capability-sweep.yml`, `sched-sweep.yml`, `power-every-axis.yml`,
`proc-sweep.yml`, `runtime-jit-sweep.yml`, `net-capability-sweep.yml`.

**What to look for:** a result for every request in the file, and a refusal that
names its reason for any that is missing.

### The backend comparison — *do two roads reach the same number?*

Where a source has more than one working backend, the same window measured
through each. `--backend force:` bypasses the resolver, which is what makes the
comparison possible at all.

`mem-backend-agreement.yml`, `mem-cgroup-counters.yml`,
`gpu-energy-two-ways.yml`, `net-two-mechanisms.yml`, `runtime-backends.yml`,
`sched-forced-backends.yml`.

**What to look for:** agreement, or a `degraded:` line that explains the
disagreement. Eight sources have two working backends today; the rest of these
sessions pin a backend that is *declared and not built*, which must exit 3 and
name what is missing rather than fail like a broken run.

### The loss path — *does it say what it did not see?*

A session driven hard enough to lose something on purpose: a ring that
overflows, a map that fills, a poll asked to run faster than its own read.

`mem-allocation-storm.yml`, `mem-retained-eviction.yml`, `sched-wakeup-loss.yml`,
`cpu-profile-loss.yml`, `gpu-poll-too-fast.yml`, `io-late-ticks.yml`,
`net-capture-accounting.yml`, `power-late-ticks.yml`, `proc-loss.yml`,
`runtime-jit-loss.yml`.

**What to look for:** the loss counted and named in the output. A run that lost
three percent and says `no loss` is the failure this whole project is built
against — so these are the sessions where output saying *nothing went wrong* is
the wrong answer.

### The refusal — *is the reason the right reason?*

**Four** different noes, which must never be confused, and which the fifth wave
gave four codes:

| It says | It means | Where it is decided | Exit |
|---|---|---|---|
| `cannot be measured per …`, `mode X cannot produce Y` | the request cannot mean anything | the validator, before resolution | **2** |
| `has no viable backend on this machine` | the request is fine and this machine cannot serve it | the resolver | **1** |
| `no collector is written for … yet` | the request is fine, this machine can serve it, and this build has not written it | the resolver | **3** |
| `cannot start collecting` | a collector was chosen and refused once attached | the collector | **1**, or **3** where the reason is a shape the collector has not written |

The middle two are the pair worth keeping apart, and the fifth wave is what
separated them: raising a privilege or changing machine fixes **1** and nothing
fixes **3**, so a harness running sessions across a fleet needs to know which it
got. `docs/fixes/runner.md` has the rule that decides a refusal at attach time.

Which of the two a given session shows **depends on the machine**, which is
worth knowing before reading the list below: a source whose only backend this
machine cannot reach answers **1** where the same source elsewhere answers
**3**. That used to be true of thirty sources here, because their only eBPF
backend was a classic tracepoint and tracefs is root-only on a stock machine.
Finding 36 of `docs/open-findings.md` closed twenty-seven of them by putting a
raw tracepoint ahead of the classic one, so the sessions below now show **3**.
The three that are left — `gpu.job`, `io.fs`, `io.readahead` — are named there
with why.

`cpu-out-of-reach.yml`, `mem-refused-not-built.yml`,
`mem-refused-unavailable.yml`, `net-not-built.yml`, `proc-not-built.yml`,
`runtime-not-built.yml`, `sched-not-implemented.yml`.

**What to look for:** whether the message names the thing that is actually
missing — the right sysctl, the right capability, the right flag. A refusal that
sends a reader to the wrong one costs more than no message.

> Two of those four are wrong, and both are recorded as defects by the
> scenarios that found them. **"No viable backend" returns 2**, the same code as
> a request that cannot mean anything, so a caller scripting around these
> cannot tell *fix your request* from *run this somewhere else*
> (`docs/scenarios/io.md`, defect 3). And **"cannot start collecting" returns
> 1**, the code for a run that failed, where the honest answer for a mode the
> collector does not implement is 3 (`docs/scenarios/io.md`, defect 1).
>
> The *messages* are right in every case, which is why the sessions above check
> those rather than the codes.

---

## Sessions that need something first

Most run anywhere. These do not, and they say so rather than printing a zero.

| Session | Needs |
|---|---|
| `sched-*.yml`, `cpu-where-the-time-went.yml` | `CAP_BPF` and `CAP_PERFMON` — `make` grants them, every `cargo build` drops them |
| `net-capture-accounting.yml` | `CAP_NET_RAW`, which the build does **not** grant |
| `gpu-*.yml` | an NVIDIA device and NVML |
| `cpu-one-busy-core.yml`, `power-core-under-load.yml` | a load started from outside, named in the file's header |
| `cpu-throttled-cgroup.yml` | a cgroup with a cpu quota, created as root |
| `power-energy.yml` (`fs:sys` backend) | root — the RAPL files were restricted after they were shown to leak what the machine was computing |

---

## Reading further

- **`docs/scenarios/<family>.md`** — one document per family: what is worth
  measuring there, which workloads guarantee what, what cannot be verified on
  this machine, and the defects the scenarios found. Sixty-three of them, none
  worked around.
- **`examples/<session>.yml`** — every session opens with the question it
  answers, the command that runs it, and what to expect. They were written to
  be read.
- **`CATALOG.md`** — the five axes and what each one means.
- **`man/omn.1`** — `make man`.
- **`STATE.md`** — what works today, measured, and what is left.
- **`docs/portability.md`** — the same tool on other kernels. `make matrix`
  boots it under 5.15, 6.1, 6.6 and 7.2 in KVM, with and without capabilities,
  BTF and tracefs, and runs every source each guest calls available;
  `scripts/matrix.sh here` is this machine alone, in about two minutes.

---

## Every session, by family

Each is `omn -f examples/<file>`. The whole family is
`scripts/demo.sh <family>`.

## cpu

| Session | The question it answers |
|---|---|
| `cpu-counters.yml` | What is this program's cpu actually doing? |
| `cpu-known-work.yml` | Does the counter count what the program actually did? |
| `cpu-one-busy-core.yml` | Is this core busy, and is it busy with my work? |
| `cpu-out-of-reach.yml` | A session that is valid everywhere and runs almost nowhere. |
| `cpu-per-core.yml` | Which core is doing the work, and what is it spending it on? |
| `cpu-profile-loss.yml` | What did the profiler not see? |
| `cpu-saturation.yml` | Is this machine busy, or is it oversubscribed? |
| `cpu-throttled-cgroup.yml` | Was this run throttled, and is it therefore worth keeping? |
| `cpu-where-the-time-went.yml` | Where did this program's time go -- on a cpu, or waiting? |

## mem

| Session | The question it answers |
|---|---|
| `mem-allocation-exact.yml` | Twenty thousand allocations of four thousand and one bytes. |
| `mem-allocation-storm.yml` | Two hundred thousand allocations: more than the pipe can carry. |
| `mem-backend-agreement.yml` | Two roads to the same number: do they arrive at it? |
| `mem-cache-lines.yml` | A scan whose cache misses are known before it runs. |
| `mem-capability-sweep.yml` | Every shape this family can produce, over one program. |
| `mem-cgroup-counters.yml` | The same two numbers, through cgroup2 instead of procfs. |
| `mem-faults-ground-truth.yml` | The program counts its own faults, and the tool has to find the same number. |
| `mem-footprints.yml` | Three footprints of one program, over one window. |
| `mem-fragmentation.yml` | What did the allocator keep? |
| `mem-machine-counters.yml` | The three machine-wide readers, and the one that reads the mode as well. |
| `mem-poll-too-fast.yml` | A cadence the collector cannot keep. |
| `mem-refused-not-built.yml` | A request that is valid, that this machine can serve, and that this build cannot collect. |
| `mem-refused-unavailable.yml` | A request this machine cannot serve, on a machine that cannot serve it. |
| `mem-retained-eviction.yml` | More freed ranges than the table can hold. |
| `mem-rss-run.yml` | A runnable session: watch one program's resident memory while it runs. |
| `mem-working-set.yml` | How much of what it holds does a program actually use? |
| `mem-allocator-ledger.yml` | The kernel's own ledger: what the page and slab allocators handed out. |
| `mem-compaction-watch.yml` | Compaction: what a machine pays for contiguity when its memory is scattered. |
| `mem-kernel-storm.yml` | The loss path: a kernel-side histogram driven past what it can hold. |
| `mem-kernel-sweep.yml` | Every shape the four kernel-side `mem` sources can produce, over one window. |
| `mem-reclaim-squeeze.yml` | Direct reclaim: the kernel doing the allocator's work inside the allocating thread. |
| `mem-load-latency.yml` | Two walks whose latency is decided by their size, sampled by the hardware. |
| `mem-false-sharing.yml` | Two processes on one cache line, then on two. |

## io

| Session | The question it answers |
|---|---|
| `io-device-load.yml` | What the disks of this machine are actually doing, over time. |
| `io-known-volume.yml` | How much did the devices actually move, and in how many requests? |
| `io-late-ticks.yml` | A poll that cannot keep up, and a tool that says so. |
| `io-per-process.yml` | Which of these bytes were mine? -- and why this build says it cannot tell. |
| `io-pressure.yml` | Is the disk the bottleneck, or is it merely busy? |
| `io-substituted-counters.yml` | Counters that lie, and what the tool does about it. |
| `io-syncs.yml` | What does a sync cost, and which ones waited for the device? |
| `io-three-layers.yml` | One program's reads, measured at three layers at once. |
| `io-device-latency.yml` | Which device was slow, and how slow -- as a distribution, never as a mean. |
| `io-known-requests.yml` | How many requests, and how big? -- checked against a number chosen in advance. |
| `io-my-own-reads.yml` | What did *this program* make the device do? |
| `io-request-halves.yml` | Where did a block request spend its time -- queueing, or on the device? |
| `io-ring-overflow.yml` | Driving the block tracer until it loses events, on purpose. |
| `io-writeback-rounds.yml` | The half of a write nobody is waiting for. |
| `io-cold-then-warm.yml` | One file read cold and then warm: every page missed once, and the second read served from the cache. |
| `io-readahead-patterns.yml` | Readahead in front of a sequential reader, and not in front of a random one. |

## net

| Session | The question it answers |
|---|---|
| `net-capability-sweep.yml` | Does every axis the `net` catalog declares actually work? |
| `net-capture-accounting.yml` | Does the capture say what it did not see? |
| `net-counters.yml` | The `count` leg of the sweep, and what a counter source actually reports. |
| `net-health.yml` | Is the network the problem, or is it us? |
| `net-injected-rtt.yml` | What does the stack believe about this path, and is it right? |
| `net-interface-drops.yml` | Did this link drop anything, and was it a drop or an error? |
| `net-late-ticks.yml` | Does a poll that cannot keep its own cadence admit it? |
| `net-not-built.yml` | Valid, and not built. Exit 3, which is not exit 1. |
| `net-snapshot.yml` | The `snapshot` leg of the sweep: one read, now. |
| `net-two-mechanisms.yml` | Two roads to the same traffic, and they have to meet. |
| `net-fentry-sweep.yml` | Does every axis the two fentry `net` sources declare actually work? |
| `net-handshake-to-close.yml` | From the SYN to the FIN: one connection, joined across two sources by its cookie. |
| `net-bpf-sweep.yml` | Does every axis the five eBPF `net` sources declare actually work? |
| `net-closed-port.yml` | Why did the machine throw that packet away? |
| `net-connection-life.yml` | How long did that connection live, and what crossed it? |
| `net-drop-storm.yml` | What happens when the machine loses packets faster than this can read them? |
| `net-queue-delay.yml` | How long did that packet wait to leave? |
| `net-retransmissions.yml` | Did this path lose anything, and did the stack notice? |
| `net-softirq-cost.yml` | Where did the cpu go while nothing was running? |

## sched

| Session | The question it answers |
|---|---|
| `sched-blocking-stack.yml` | Not how long a program waited -- where. |
| `sched-forced-backends.yml` | Every backend this family declares and does not implement, pinned on purpose. |
| `sched-half-blocked.yml` | A program that is half on the cpu and half waiting, on purpose. |
| `sched-migrations.yml` | A task driven from core to core, a known number of times. |
| `sched-not-implemented.yml` | `sched` sources this build describes and cannot measure. |
| `sched-round-trips.yml` | One workload, one number, four sources that must all find it. |
| `sched-sweep.yml` | Every axis the catalog offers this family, exercised once. |
| `sched-wakeup-loss.yml` | Drive the wakeup ring past what it can hold, and check the tool says so. |
| `sched-wakeup.yml` | Who unblocks whom. |
| `sched-futex-known-wait.yml` | A lock held for a known time, and the thread that waited for it. |
| `sched-irq-against-procfs.yml` | How much of this machine goes to interrupt handlers, and to which devices? |
| `sched-lock-shared-file.yml` | What does sharing one file cost, in kernel lock contention? |
| `sched-numa-balance.yml` | The balancer giving up on a task, on a machine that has two places to put it. |
| `sched-sweep-machine.yml` | Every axis the catalog offers this family's machine-wide sources. |
| `sched-mutex-known-hold.yml` | Two threads, one pthread mutex, a known number of acquisitions held for a known time. |
| `sched-mutex-uncontended.yml` | The same program with one thread: the same holds, and waits of nothing. |
| `sched-mutex-with-heap.yml` | Lock waits and allocations of one program, through one shim. |
| `sched-mutex-loss.yml` | Mutex records produced faster than the pipe drains, and every one accounted for. |

## power

| Session | The question it answers |
|---|---|
| `power-confounders.yml` | The guardrail: what else was happening to the processor while you measured. |
| `power-core-under-load.yml` | Was core 1 busy, and was it busy at speed? |
| `power-energy.yml` | What a window of work cost, in joules. |
| `power-every-axis.yml` | Every axis this family declares and this build can serve, in one run. |
| `power-late-ticks.yml` | The loss path: a pace the collector cannot keep, and whether it says so. |
| `power-load-on-one-core.yml` | The load, and the independent oracle for what the next session will measure. |
| `power-throttle.yml` | The refusal: did this run throttle, and if the tool cannot tell, does it say |

## gpu

| Session | The question it answers |
|---|---|
| `gpu-device-state.yml` | What the device did while a kernel was resident on it. |
| `gpu-energy-two-ways.yml` | Two roads to the same joules, and what they say about each other. |
| `gpu-idle-baseline.yml` | The same four device figures, with the GPU left alone. |
| `gpu-job-latency.yml` | How long a command buffer spent between submission and its fence. |
| `gpu-poll-too-fast.yml` | A poll asked to run faster than the driver can answer. |
| `gpu-vram-of-a-process.yml` | Zero video memory held, reported as zero. |
| `gpu-vram-step.yml` | Video memory a program asked for, checked to the byte. |
| `gpu-api-cuda.yml` | How long the host spends inside the CUDA driver API, call by call. |
| `gpu-api-vulkan.yml` | How long the host spends submitting to a Vulkan queue and waiting for it. |
| `gpu-api-unreachable.yml` | A program that drives the GPU where the shim cannot see it, and says so. |

## proc

| Session | The question it answers |
|---|---|
| `proc-exit.yml` | A program that ends is not a program that lost data. |
| `proc-handles.yml` | Is this program running out of handles? |
| `proc-loss.yml` | Did the poller keep up, and does it say so when it did not? |
| `proc-not-built.yml` | The four `proc` sources this build does not implement yet. |
| `proc-one-reading.yml` | What is this process holding, right now? |
| `proc-sweep.yml` | Does every axis these two sources declare actually work? |
| `proc-bpf-sweep.yml` | Does every axis the four event sources declare actually work? |
| `proc-lifecycle.yml` | What ran on this machine, and what became of it? |
| `proc-signal-storm.yml` | When the ring overflows, does the tool say so -- and by how much? |
| `proc-syscall-sweep.yml` | Does every axis `proc.syscall` declares actually work? |
| `proc-syscall.yml` | How long did this program spend inside the kernel, and in which calls? |

## runtime

| Session | The question it answers |
|---|---|
| `runtime-backends.yml` | Two roads to the same number -- where there are two. |
| `runtime-jit-loss.yml` | Does this source say what it did not see? |
| `runtime-jit-sweep.yml` | Does every axis `runtime.jit` declares actually work? |
| `runtime-jit.yml` | What did the runtime compile, and when? |
| `runtime-not-built.yml` | The three `runtime` sources this build does not implement, and the backend of |
| `runtime-usdt-gc.yml` | How long did the collector stop the program? |
| `runtime-usdt-alloc.yml` | What did the runtime allocate, and how much of it? |
| `runtime-usdt-probes.yml` | A probe you name, in a program that publishes it. |
| `runtime-usdt-libc.yml` | The probes libc was built with, in a program that did not ask for them. |
| `runtime-usdt-threads.yml` | A probe follows the process, not the thread that started it. |
| `runtime-usdt-flood.yml` | More hits than the ring holds, and every one of them accounted for. |
| `runtime-usdt-absent.yml` | A runtime built without its probes is refused, and says what it does carry. |

## vertical

`ROADMAP.md` phase 4: one request followed from where it entered to where it
touched the disk, by the id it carried. A real server on loopback, delays
injected where the test chose them, and each landing in its own row.

| Session | The question it answers |
|---|---|
| `vertical-one-request.yml` | Where did one request's latency go, boundary by boundary, against injected delays? |
| `vertical-abandoned.yml` | Is a request the server never ended counted and named, or dropped? |

## attach

`ROADMAP.md` phase 6, task 4: a request whose window is its own, attached and
detached inside a run that keeps going. Each session checks that a source
counts what happened while it was attached and nothing outside it.

| Session | The question it answers |
|---|---|
| `attach-late-probe.yml` | Does a kernel probe loaded mid-run see what happened after it and nothing before? |
| `attach-one-phase.yml` | Does a source attached for one phase of a program count that phase and nothing of the other? |
| `attach-escalate-phases.yml` | Can a run decide which layer to pay for from its own cheap numbers? A program cpu-bound then lock-bound gets the profile, then the futex waits, one layer at a time. |

## triage

`ROADMAP.md` phase 10: the layer above the engine, which asks it a question
for an objective and names what dominates. Each is the session `omn latency`
or `omn saturate` runs, against a workload whose limit the test chose.

| Session | The question it answers |
|---|---|
| `triage-latency-cpu.yml` | Why is a cpu-bound program slow? The session `omn latency` asks through: on-cpu dominates, and the hottest function is named. |
| `triage-latency-io.yml` | Why is a program making synchronous writes slow? Its blocked time is uninterruptible, from fdatasync: the disk. |
| `triage-latency-lock.yml` | Why is a thread taking a contended mutex slow? Its sleep is covered by pthread_mutex_lock waits, and the mutex is named. |
| `triage-latency-tail.yml` | What makes the slowest of five runs slow, when the median is cpu-bound? The one run's 200 ms sleep. |
| `triage-saturate-queue.yml` | What do four threads on one cpu queue for? The cpu, three deep, by Little's law over `sched.runqlat`. |

## join

The eighth wave's join engine, each session showing one thing it can say that it
could not before: a correlation that is `TEMPORAL` by construction and says so,
and a decomposition that adds up — or prints what does not, as a signed residue.

| Session | The question it answers |
|---|---|
| `join-between-residency.yml` | What moved the resident size, asked by key and by the clock — and a decoy the clock gets wrong, at TEMPORAL. |
| `join-decompose-memory.yml` | The rise in resident size split into live and retained bytes, with a signed residue the program predicts. |
| `join-decompose-compute.yml` | One task's wall clock split into on-cpu, runnable and blocked, each row's kind and resolution, and its releasers ranked by chain. |

## cross-family

A crossing asks what no single family can answer, so it belongs to none of them
and carries the `cross-` prefix instead of a family's. Each one states the
evidence behind every joined line — `IDENTITY` > `KEY` > `WAKEUP` > `TEMPORAL` —
and `docs/scenarios/cross-<name>.md` says what it found.

| Session | The question it answers |
|---|---|
| `cross-memory-which-allocation.yml` | Which allocation cost memory, and which of them stayed? |
| `cross-memory-did-it-stay.yml` | Did it stay? What the allocator kept after the program gave it back. |
| `cross-compute-where-the-time-went.yml` | Where did one task's wall clock go — on a cpu, waiting for one, or blocked? |
| `cross-compute-three-states.yml` | The same task with all three states large at once, and the rows adding up. |
| `cross-compute-queued-after-wakeup.yml` | A known amount of sleep, and the part of "blocked" that was not. |
| `cross-compute-drained-wakeup.yml` | What a wakeup join onto a drained total attributes, and why. |
| `cross-compute-lossy-profile.yml` | The same decomposition from a run that lost samples, to read beside a clean one. |
| `cross-io-one-read.yml` | What did one read cost, from the block layer to the reader waking up? |
| `cross-io-stale-pointer.yml` | Would a bare pointer have credited somebody else's request to this reader? |
| `cross-machine-frequency.yml` | Did the machine's clock explain the instruction rate you just measured? |
| `cross-machine-reclaim-io.yml` | Was the I/O you just measured the program's, or the memory limit's? |

## general

| Session | The question it answers |
|---|---|
| `allocation-cost.yml` | Which allocation actually cost memory? |
| `web-latency.yml` | A latency budget for one web server process: where its 400 ms actually went. |
| `filters-and-formats.yml` | The same measurement, four ways out. |
| `saturation.toml` | A cheap, always-on saturation sweep: what is this machine limited by? |
