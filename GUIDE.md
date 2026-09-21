# Guide — running the demonstrations, and what each one shows

Seventy-nine sessions, nine families, four kinds of question. This is how to
run them and what to look for.

Everything here runs against **this** machine, now. Nothing is recorded, nothing
is staged, and a session this machine cannot serve says so rather than printing
something plausible — which is half of what there is to see.

```console
make                    # build, and grant the capabilities a relink drops
make demo               # the guided tour: eight questions, about seven seconds
scripts/demo.sh --list  # every session, by family, without running any
scripts/demo.sh mem     # one family's sessions
make demo-all           # the tour and then all seventy-nine, about ninety seconds
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

## io

| Session | The question it answers |
|---|---|
| `io-device-load.yml` | What the disks of this machine are actually doing, over time. |
| `io-known-volume.yml` | How much did the devices actually move, and in how many requests? |
| `io-late-ticks.yml` | A poll that cannot keep up, and a tool that says so. |
| `io-per-process.yml` | Which of these bytes were mine? -- and why this build says it cannot tell. |
| `io-pressure.yml` | Is the disk the bottleneck, or is it merely busy? |
| `io-substituted-counters.yml` | Counters that lie, and what the tool does about it. |

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

## proc

| Session | The question it answers |
|---|---|
| `proc-exit.yml` | A program that ends is not a program that lost data. |
| `proc-handles.yml` | Is this program running out of handles? |
| `proc-loss.yml` | Did the poller keep up, and does it say so when it did not? |
| `proc-not-built.yml` | The four `proc` sources this build does not implement yet. |
| `proc-one-reading.yml` | What is this process holding, right now? |
| `proc-sweep.yml` | Does every axis these two sources declare actually work? |

## runtime

| Session | The question it answers |
|---|---|
| `runtime-backends.yml` | Two roads to the same number -- where there are two. |
| `runtime-jit-loss.yml` | Does this source say what it did not see? |
| `runtime-jit-sweep.yml` | Does every axis `runtime.jit` declares actually work? |
| `runtime-jit.yml` | What did the runtime compile, and when? |
| `runtime-not-built.yml` | The three `runtime` sources this build does not implement, and the backend of |

## general

| Session | The question it answers |
|---|---|
| `allocation-cost.yml` | Which allocation actually cost memory? |
| `web-latency.yml` | A latency budget for one web server process: where its 400 ms actually went. |
| `filters-and-formats.yml` | The same measurement, four ways out. |
| `saturation.toml` | A cheap, always-on saturation sweep: what is this machine limited by? |
