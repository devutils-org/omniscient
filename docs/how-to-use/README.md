# How to use `omn`, family by family

This directory is for the person running the tool. It answers "I have this
question about my program or my machine: which source do I ask, on which
target, in which mode, and what will it cost?" One page per family, and this
page for what every family shares.

It is not the reference. The catalog is (`catalog/*.yaml`, projected by `omn
list`), and when this guide and `omn list <source>` disagree, `omn list` is
right and this page is stale. `docs/CATALOG.md` is the vocabulary in full,
`man/omn.1` every flag, `docs/GUIDE.md` the demonstrations.

| Family | The questions it answers | Page |
|---|---|---|
| `cpu` | where the cpu time goes, how well the core runs it, is the machine busy | [cpu.md](cpu.md) |
| `mem` | how much memory, who allocated it, is it growing, is the machine short | [mem.md](mem.md) |
| `io` | how long reads and writes take, at which layer, how busy the disk is | [io.md](io.md) |
| `net` | connection and socket latency, volume, drops, one request's life | [net.md](net.md) |
| `sched` | why a task is not running: blocked, queued, contended, woken by whom | [sched.md](sched.md) |
| `power` | energy, effective frequency, idle states, throttling | [power.md](power.md) |
| `gpu` | device busy, clocks, VRAM, kernel time, host-side API cost | [gpu.md](gpu.md) |
| `proc` | syscalls, process life, descriptors, threads, signals | [proc.md](proc.md) |
| `runtime` | GC pauses, JIT, runtime allocations, your own probes | [runtime.md](runtime.md) |

To show `omn` to someone, follow [demo.md](demo.md): ten minutes, six acts.
To check `omn` against programs of known shape -- before a demo, after a change, on a new
machine -- run the application suite: [apps.md](apps.md), `make apps`.

---

## The shortest path: a family and a target

If you do not know which source you want, name the family:

```console
omn cpu -- ./prog                 # where does it spend its cpu time
omn mem -- ./prog                 # how much memory, and does it grow
omn io --cost med -- ./prog       # how long it waits on reads and writes
omn sched --pid 1234 -d 5s        # how often it is switched out, how long it is off a cpu
```

That is the **lazy mode** (`docs/lazy.md`): each family declares, as catalog
data, the minimum set of requests that answers its first question, per cost
band. `--cost low|med|high` picks the band (absent: the cheapest). The run
prints the requests it composed in long form first, so you can copy one and
start tuning it. `omn list` ends with every family's defaults.

When the lazy mode stops being enough, write the request yourself. Everything
below is what you then choose.

---

## The five axes of a request

```console
omn trace <source> <target> -m <mode> -e <emit> -k <keys> [--filter <expr>] [-d <duration>]
```

`omn <source> ...` is short for `omn trace <source> ...`. Several sources,
comma- or space-separated, are measured over **one** window and one launch:
`omn trace cpu.instructions,cpu.cycles -- prog` is `perf stat -e
instructions,cycles -- prog`.

### Source: what to measure

`<family>.<metric>`: `io.latency`, `cpu.profile`, `net.tcp.rtt`. Find one with:

```console
omn list                 # every source, with HERE (this machine) and COLLECTOR (this build)
omn list mem             # one family
omn list io.latency      # one source in full: modes, emits, targets, keys, backends, caveats, why it is or is not usable here
```

Read the caveats of a source before trusting its number. They are where the
catalog writes down what the number is *not*.

### Target: what to observe

| Target | Flag | Meaning |
|---|---|---|
| a program to launch | `-- ./prog args` | the program and the tree it starts, from before its first instruction (it is held at a gate until every collector is attached) |
| an existing process | `--pid 1234` | that process; most tracing sources follow the children it starts afterwards |
| an existing thread | `--tid 1235` | that thread alone |
| a cgroup | `--cgroup /sys/fs/cgroup/...` | every task in it and below it |
| a container | `--container NAME` | resolved to the cgroup its runtime (docker, podman, nerdctl) names |
| cores | `--cpu 0-3,8` | what runs on those cores, whoever it is |
| an interface | `--iface eth0` | `net.*` only |
| the machine | `--system` | everything |

**A program beside another target is the window, not the target**:
`omn trace power.energy --system -- ./bench` measures the machine for as long
as `./bench` runs, as `perf stat -a -- prog` does.

Each source says which targets it carries (`omn list <source>`, `targets`):

- `native`: served by the backend's own filtering.
- `bpf_only`: served only through an eBPF backend, which filters inside the
  kernel program. Without the capabilities (below) that target is refused.
- absent or `no`: refused, with the reason. This is deliberate: a machine-wide
  number (RAPL, DRAM bandwidth, `/proc/diskstats`) is **never** presented as a
  process's. `power.energy --pid` is refused rather than answered.

Some sources are machine-scoped by nature and only take `--system` (or
`--cpu`, `--iface`): `mem.system`, `io.util`, `sched.runqueue`,
`power.*`, `net.drops`. To scope them to a program, give the program as the
window: `omn trace io.util --system -- ./prog`.

Duration: `-d 5s`. Beside a launched program or a `--pid`, it is a ceiling;
the target's exit ends the run.

### Mode: how to observe

| Mode | What it does | Typical use | Valid emits |
|---|---|---|---|
| `count` | one counter over the window | totals, rates, `perf stat` | `rate`, `summary`, `raw`, `topk` |
| `sample@997hz` | a sample every so often | profiles with stacks | `stack`, `topk`, `hist`, `raw` |
| `trace` | every event | latency distributions, per-request records | `raw`, `hist`, `topk`, `timeline`, `stack`, `rate`, `summary` |
| `poll@100ms` | re-read a gauge periodically | memory curves, utilization, temperature | `raw`, `timeline`, `summary` |
| `snapshot` | one read now | "what is it right now" | `summary`, `raw` |

Each source declares the modes it accepts, and a mode it does not is refused.
Two rules worth knowing:

- **`poll` has no `hist`**, on purpose: a histogram of a polled gauge is a
  histogram of when the poller looked, and it hides the late ticks that
  lose the slow moments.
- `trace` is the dearest mode. Where a source keeps its distribution in the
  kernel (`io.latency -e hist`, `sched.offcpu`, `proc.syscall`), an event is a
  histogram **row**, not a call: its `count` says how many calls it stands for.

### Emit: what comes out

| Emit | Gives |
|---|---|
| `summary` | count, min, mean, max, last, and a total where the values add up |
| `hist` | a distribution (`hist:log2` by default, `hist:linear:WIDTH`) |
| `topk:N` | the N heaviest series |
| `rate` | per second over the window |
| `timeline` | the series over time |
| `stack` | stacks (flame graph material) |
| `raw` | every event, every attribute |

`summary@<attr>`, `topk:N@<attr>` and `hist@<attr>` group by an attribute's
values instead of by series: `omn trace proc.syscall -m count -e
topk:10@syscall --system -d 5s`. The attribute must be one of the source's
`filter_fields`.

### Keys and filter

`-k` attaches correlation keys (`task`, `file`, `sock`, `blk`, `dev`, `dir`,
`syscall`, `comm`, `cgroup`, `cpu`, ...). A key also splits the series:
`-k dev,dir` gives one series per device and direction. Each source lists the
keys it carries, and a key it does not carry is refused.

`--filter` takes a predicate over the source's `filter_fields`, pushed into
the kernel when the backend allows it:

```console
omn trace io.latency --system -m trace -e hist --filter 'bytes >= 65536' -d 10s
omn trace proc.calls -m trace -e raw --filter "syscall == 'openat'" -- ./prog
```

---

## Backends: one source, several roads

A source is a chain of backends, best first. `omn list io.latency` prints it:

```
1. bpf:raw_tp   cost med    per request, per task
2. bpf:tp       cost med    equivalent
3. fs:tracefs   cost med    degraded: no per-task attribution
4. fs:proc      cost free   degraded: per device, not per request
```

The resolver takes the **first viable one**: available on this machine (probed,
and unknown never counts as available), with a collector in this build, and
able to serve your target, mode and emit. A fallback either says what it
degrades or declares itself equivalent, and the run prints the degradation in
its notes. You do not choose a backend; you choose a request, and the run tells
you which road it took and what that road cannot see.

What makes the first rungs unavailable is usually privilege:

- **eBPF and per-cpu perf backends need `CAP_BPF` and `CAP_PERFMON` on the
  binary.** `make` grants them, and **every relink drops them**: a `cargo
  build` silently turns an eBPF source into its procfs fallback or a refusal.
  `getcap target/debug/omn` says whether they are there.
- `net.capture` also needs `CAP_NET_RAW` (`make CAPTURE=1`), off by default.
- Some files are root's (`/sys/class/powercap/*/energy_uj`, tracefs, `/proc/slabinfo`).

`--backend force:<id>` pins a backend. It exists for the comparison bench;
in ordinary use, leave the choice to the resolver.

To see the whole decision without measuring anything:

```console
omn trace io.latency --pid 1234 -m trace -e hist --dump-request    # what resolves, and why each backend was passed over
omn trace io.latency --pid 1234 -m trace -e hist --dump-resolved   # the record to keep beside the numbers
```

---

## Cost: what a measurement costs the program

Each backend declares a band:

| Band | Order of magnitude |
|---|---|
| `free` | a counter read, ~0 |
| `low` | under 1 % |
| `med` | 1 to 5 % |
| `high` | 5 to 20 % |
| `extreme` | over 20 %, unbounded: something runs inside the target (the preload shim, a program on every syscall, kernel replay) |

Cost is **throughput-dependent**: a tracepoint at 1 kHz is cheap and the same
one at 2 MHz is not. Two tools hold it:

- `--cost low|med|high` (lazy mode only) chooses **which requests** a family's
  default composes.
- `--budget 5%` is a **ceiling**, measured during the run against the target's
  own cpu time. Over it, the run moves to a cheaper backend of the chain, then
  from `trace` to `sample`, or stops, and says which.

Every run ends with a `cost:` line: what the collection spent, against what
the target spent, and whether that is inside the declared band.

How to keep a measurement cheap:

1. Prefer `count` or `poll` when the question is a total or a level.
2. Prefer a source that aggregates in the kernel (`-e hist`, `-e summary`)
   over `-e raw` on a high-rate event.
3. Narrow the target (`--pid`, `--filter`): an eBPF program on a narrowed
   target still fires on every event of the machine, but it leaves early.
4. Watch `extreme` sources (`proc.syscall`, `proc.calls`, `mem.heap.*`,
   `sched.mutex`, `gpu.kernel`): they change the program they measure.

---

## Reading a run

Every run prints, around its numbers:

- **`via <backend>`**: the road taken.
- **`degraded:`**: what that road cannot see.
- **`LOSS:`** or `no loss`: dropped events, failed reads, late ticks. A count
  marked `>=` is a lower bound because something was lost.
- **`caveat:`**: the source's catalog caveats, the ones that apply.
- **`cost:`**: what the measurement cost.

A refusal is a result too: it says why, and what the source does accept.

---

## Output and files

`--format text` (default), `json`, `yaml`, `toml`, `ndjson`, and for stacks
and timelines `folded` (flamegraph.pl, inferno), `speedscope`, `pprof`,
`chrome` (Perfetto). A format that cannot carry the emit is refused. `omn
formats` lists them.

A launched program shares `omn`'s stdout and stderr. To pipe `omn`'s document
into `jq` or `yq`, send the program's output elsewhere:

```console
omn trace cpu.cycles -m count -e summary --format yaml --program-stdout ls.out -- ls -R | yq
omn trace cpu.cycles -m count -e summary --format json --program-stdout prog.log --program-stderr prog.log -- ./prog | jq
```

With `trace -r N`, the program is launched afresh and each redirected stream
is also kept as `FILE.1` through `FILE.N` when FILE is a regular file
(devices such as `/dev/null` remain discard-only). `FILE` holds the final run. The same
path for stdout and stderr keeps both streams together, including in each
numbered copy. These rules also apply to `OMN_PROGRAM_STDOUT` and
`OMN_PROGRAM_STDERR`.

Request windows accept decimal durations, for example `window: "3.9s.."`
(equivalent to `3900ms..`). Durations must represent an exact whole number of
nanoseconds. If a collector cannot attach when its window opens, the failure
is reported and the command exits nonzero. The attach failure does not stop
a launched workload; it is allowed to exit normally, or runs to an explicitly
requested duration.

`-o DIR` writes the results and the resolved file (machine, kernel, backend,
every choice) beside them, and then `omn`'s stdout is free for the program. Keep both: a number without the resolved file
cannot be compared with another machine's.

Sessions as files: anything on the command line can be a YAML, JSON or TOML
session (`omn -f examples/mem-footprints.yml`, `omn validate file.yml`).
`examples/` holds a hundred and seventy of them, each with the question it
answers in its header.

---

## Beyond one source

| Command | What it answers |
|---|---|
| `omn latency -- ./prog` | where one task's wall clock went (on a cpu, runnable, blocked) and what dominates |
| `omn saturate -- ./prog` | utilization, saturation and errors per resource |
| `omn diff BEFORE AFTER` | which function is the difference between two profiles |
| `omn top [-- ./prog]` | an interactive view of everything, live or from a recording |
| `omn join -f rules.yml RESULTS` | apply join rules to a stored run |

`docs/triage.md` explains `latency` and `saturate`, `docs/view/` the views.

---

## Inside a container

`omn` works from inside a pid or cgroup namespace (an OrbStack machine, a
container with the capabilities): targets are translated into the kernel's
ids, and the kernel's ids are translated back, so `task:<tid>` is the id you
see in `ps`. An id it could not translate (a task that exited before it was
seen, or one outside your namespace) stays the kernel's, and the run's notes
count them. `docs/keys.md` has the detail.
