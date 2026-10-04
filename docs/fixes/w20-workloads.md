# The twentieth wave, stream workloads

Branch `feat/w20-workloads`. Owns `view/workloads/**` and
`tests/view_workloads.rs`, both new.

## What landed

- **A workload per view, and one for `process`** (`view/workloads/<view>/`):
  `workload.py`, `manifest.json`, `session.yml`, and a shared `common.py`
  (the manifest, a thread's name, a burn on the thread's own cpu clock, a
  file written or read through a chosen descriptor, a scratch directory under
  `target/omn-view-workloads/`). Each program reads its parameters from its
  manifest, so the truth and the program cannot drift apart. Python, fixed
  sizes, no randomness, 1 to 3 s each, no privilege. `view/workloads/README.md`
  tabulates what each one makes known.
- **The request workload is C**, not Python: a request is declared by USDT
  probes, which a Python program cannot carry. It is the vertical case's
  server (`examples/vertical/server.c`, unchanged, built by the test into
  `target/omn-view-workloads/request/server`) given a plan of sixteen
  requests, four kinds of four: plain, behind a 30 ms lock, reading 32 cold
  pages, sent by a client that pauses 40 ms.
- **The lanes workload drives the GPU** through `tests/gpu_workload.py`'s
  driver calls (no toolkit), in a context whose synchronisation blocks: 12
  `spin` launches one at a time after three lanes take six turns of 80 ms in
  order. A machine with no device runs the lanes alone and says so.
- **`tests/view_workloads.rs`**, 13 tests: three that need no privilege (every
  view in the registry, and `process`, has a manifest naming its view, a
  program that exists and a session; every source a view needs is asked by
  its session or excused, with a reason, in the manifest's `not_exercised`;
  every session is understood by `--dump-request`), and one per workload
  holding its sources to its manifest. Each run's results are kept as
  `target/omn-view-workloads/<view>.json`, for reading a failure after the
  fact.

## Shown versus reasoned

Shown, on this machine with the capabilities, 10 consecutive green runs of the
whole file with the other streams' agents loading the machine (and alone runs
of each session, read by hand):

- **flame**: `heavy` 795, `light` 200, `medium` 402 samples for 0.80, 0.20,
  0.40 s; `nap` 0.64 s off the cpu for 0.60 s. Held: each within 25 % of
  seconds x 997, heavy/light in [3, 5], every sleep at least its seconds,
  burning functions never off the cpu (< 50 ms).
- **timebar**: `worker` blocked 800.3 ms (0.40 asleep + 0.40 on the lock),
  futex 400.3 ms, run queue 298 ms behind the pinned rival for 0.30 s, on-cpu
  0.91 s. Held: blocked in [0.98x, +0.3 s], futex >= 0.95x unless a drop was
  counted, run queue >= half, on-cpu within [0.95x, +0.6 s], `compute`'s
  samples within 25 %.
- **wake**: 300, 300, 299 wakeups on the ring's three edges, none backwards.
  Held: each edge in [0.5x, 1x] of 300 (266 and 268 were seen under load: a
  reader still runnable when its byte comes is not woken), backwards <= 2 %.
- **flows**: exactly 8 MiB written and 8 MiB read on descriptor 40, 1 MiB
  written on 41; exactly 4 MiB sent and 1 MiB received on the socket. Held
  exactly, unless the source counted a drop (then a lower bound). The
  machine's devices and `lo`: at least what was written and exchanged.
- **heatmap**: 600 receives at 1.57 to 1.60 ms, 50 at 12.1 ms, in that order.
  Held: 650 receives exactly (or a counted drop), >= 80 % under the regimes'
  geometric mean, all 50 slow ones at least the pause, >= 90 % of each fast
  phase's receives before and after the slow phase's 10th and 90th
  percentile.
- **ledger**: on-cpu 0.58 s for 0.5, resident +67.4 MB for 64 MiB, 18 329
  faults for 16 384 pages, 4 MiB on descriptor 40, 2 MiB and 256 KiB on the
  socket. Held as the manifest's trust says.
- **system**: on-cpu >= 1.0 s (the program's half), the machine's busiest
  poll >= 1.5 cores' share, devices >= 16 MiB, `lo` >= 8 MiB, a pressure
  point.
- **lanes**: lanes blocked 805, 884, 966 ms (>= 0.80 s each), 13 to 27
  switches each (>= 6), 12 kernels exactly. Held, and each lane woken by the
  one before it >= 90 % of its turns.
- **request**: 16 requests ended; lock 30.1 ms asleep with the holder named,
  block_io 32 blocks and 2 to 8 ms of disk, net_rx 20.2 ms for the old 20 ms
  pause. Held: lock in [0.75x, 1.5x], 32 blocks and some disk, net_rx in
  [0.95x, 1.5x], and the other kinds under half the lock and half the pause.
- **process**: `crunch` 604 and `tidy` 150 samples, `doze` 303 ms and
  `queue_up` 250 ms off the cpu, futex 253 ms, resident +34.6 MB for 32 MiB,
  the file and socket bytes exact.

Reasoned, not shown:

- That the record stream's `omn top --headless` recordings of these programs
  draw what the manifests say: `tests/view_truth.rs` is theirs. The sessions
  here ask each view's needs with `raw` where a test adds bytes or states up;
  the full needs a view asks live are a superset, and nothing here has run
  under `omn top`.
- That a machine with no NVIDIA device runs the lanes test without the
  kernels: the session is cut at its last request, which the test does when
  `/dev/nvidia0` is absent. Not run on such a machine.

## What I could not do, and why

- **Sources not given a known load** are named, with the reason, in the
  manifests' `not_exercised` (and the test fails if a view gains a need that
  is neither asked nor excused): `io.latency` (a device's latency is the
  device's), `sched.runqlat` and `gpu.kernel` for the heatmap;
  `net.tcp.retransmit` (needs a lossy link, which needs privilege), `gpu.mem`,
  `gpu.kernel`, `power.energy` for the ledger; `gpu.engine_busy`, `gpu.mem`,
  `power.energy` for the system view. A GPU footprint and a kernel regime for
  the heatmap are the next additions, on the lanes workload's device code.
- **The `raw` the family streams add this wave** (`cpu.util`, `io.bytes`,
  `io.syscall`, `cpu.oncpu_time`) is not asked here: it is not on `main` yet,
  and the brief leaves it to record's test.
- **One flame failure** in the first batch of runs, before the polled
  workloads got their tail; its message was not captured, and every whole-file
  run since was green. If it returns, `target/omn-view-workloads/flame.json` holds the
  run.

## Privileged runs wanted

None beyond what `make` grants: every test here ran on this machine with
`CAP_BPF` and `CAP_PERFMON` on `target/debug/omn`. After the merge, once:

```console
make && cargo test --test view_workloads -- --nocapture
```

`--nocapture` shows the lines a counted drop prints (`... with N events
counted as dropped`), which is what a busy machine produces.

## Decision lines for DECISIONS.md

- **A view workload reads its parameters from its manifest.** The manifest is
  the truth a test holds the tool to and the program's input at once, so
  changing a figure changes what the program does and what it is held to in
  one place. Leaving it open costs a second copy of every figure, in the
  program, to keep in step by hand.
- **Threads that must not wait for each other run in turn, or in separate
  processes.** Two Python threads burning at once wait on the interpreter's
  lock, an off-cpu time nobody chose (seen: 781 ms of `burn` off the cpu in
  the first flame run). The flame workload's threads run in turn; the wake
  ring, the rival, the peers are processes.
- **A polled source gets a lead and a tail.** A workload whose cost a polled
  source must see idles `lead_s` before its first phase and `tail_s` after its
  last (0.3 s and 0.6 s, at a 250 ms poll): a poll's first read is its
  baseline, and a poll after the target's exit cannot read it. Declared in the
  manifests. See the two findings below.
- **An exact figure is held exactly unless the source counted a loss**, and
  then as a lower bound (`exact_unless_counted`). Seen: `net.socket` dropped
  two receives' exit halves while another test attached a tracer to the same
  kernel function, and said so.

## Needs from another stream

- **cpu (`cpu.oncpu_time` on `fs:proc`)**: on a launched program it reads the
  launched process alone, not its children (a child burning 1.0 s added
  nothing; the system workload's manifest says so). And the cpu time between
  the last 250 ms poll and the target's exit is in no point: the system
  workload, ending on its cpu, measured 0.93 s for at least 1.0 s until it got
  a 0.6 s idle tail. Either is worth a sentence in the source's caveat, and
  the second a closing read at the target's exit while `/proc/<pid>` is still
  there (zombie) if the runner can make one.
- **io (`io.bytes` on `fs:proc`, polled, system scope)**: the sum of a run's
  timeline points was below what the run fsynced to the device under
  `target/` -- 1.99 MB and 4.69 MB for 9 MiB -- in two of a dozen runs of the
  flows workload, which then wrote at once on start. With a 0.3 s idle lead it
  has not happened since. `diskstats.rs` takes a polled run's baseline at its
  first tick, not before the program is released, so what moved before that
  tick is in no point; if that is the cause, a counted run's early baseline
  (`start`) would serve a polled run too, its first point then covering
  [release, first tick].
- **record**: these sessions are the sources the tests hold; a recording for
  the views wants each view's full needs (`omn top --headless -o`), launched
  as each manifest's `run` says (`-X perf` for flame, timebar and process; the
  request server built first, as `request/session.yml`'s header says).
  `tests/view_workloads.rs`'s helpers (`python_weights`, `bytes_by_op_fd`,
  `exact_unless_counted`) are there to copy into `tests/view_truth.rs`.
- **view**: the `process` view is not in the registry yet; the inventory test
  adds it to the list by hand until it is, and then requires nothing more.
