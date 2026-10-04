# View workloads

One program per view, and one for `process`, whose load is known in advance
and exercises what that view can draw. Each directory holds:

- `workload.py` -- the program (the request view's is the vertical case's C
  server, `examples/vertical/server.c`, because a request is declared by USDT
  probes a Python program cannot carry);
- `manifest.json` -- what the program does, per phase, thread, function,
  request, file, socket and kernel, and under `trust` how exact each figure is.
  The program reads its parameters from it, so the two cannot drift apart.
  `not_exercised` names each source the view needs that this workload cannot
  give a known load, and why;
- `session.yml` -- the view's sources asked of the program, as
  `omn -f view/workloads/<view>/session.yml` runs them from the repository's
  root.

`common.py` holds what they share: the manifest, a thread's name, a burn of
cpu time measured on the thread's own clock, a file written or read through a
chosen descriptor (so a source that reports bytes per descriptor reports the
file apart from the interpreter's own reads), and a scratch directory under
`target/omn-view-workloads/`, on a disk.

`tests/view_workloads.rs` runs each session and holds the sources to the
manifest -- the workload's ground truth, before any view draws it. Each run's
results are kept as `target/omn-view-workloads/<view>.json`.

| Workload | What is known | Sources it is held on |
|---|---|---|
| `flame` | `heavy` 0.80 s and `light` 0.20 s on `hot`; `medium` 0.40 s on and `nap` 0.60 s off on `warm`; the threads run in turn | `cpu.profile` (under `-X perf`), `sched.offcpu` |
| `timebar` | `worker`: 0.60 s on cpu, 0.40 s asleep, 0.40 s on a lock, 0.30 s runnable behind a rival on its core | `cpu.profile`, `cpu.oncpu_time`, `sched.offcpu`, `sched.runqlat`, `sched.futex` |
| `wake` | a ring of three processes, 300 hand-offs per edge, none backwards | `sched.wakeup` |
| `flows` | 8 MiB written and read, 1 MiB written, each on its own descriptor; 4 MiB sent and 1 MiB received on a socket | `io.syscall`, `net.socket`, `io.bytes` and `net.bytes` on the machine |
| `heatmap` | 300 receives waiting 1.5 ms, 50 waiting 12 ms, 300 at 1.5 ms again | `net.socket` |
| `ledger` | 0.5 s of cpu, 64 MiB touched and held, 4 MiB to a file, 2 MiB and 256 KiB on a socket | `cpu.oncpu_time`, `mem.rss`, `mem.faults`, `io.syscall`, `net.socket` |
| `system` | the program and a child burning 1.0 s each at once, 16 MiB fsynced, 8 MiB over loopback | `cpu.oncpu_time`, and the machine's `cpu.util`, `mem.pressure`, `io.bytes`, `net.bytes` |
| `lanes` | three lanes taking 6 turns of 80 ms in order; 12 `spin` kernels one at a time (where there is a device) | `sched.switches`, `sched.offcpu`, `sched.wakeup`, `gpu.kernel` |
| `request` | 16 requests: 4 plain, 4 behind a 30 ms lock, 4 reading 32 cold pages, 4 from a client pausing 40 ms | `net.request` |
| `process` | all of it on one process: `crunch` and `tidy`, a sleep, a lock, 32 MiB, a file, a socket | `cpu.profile`, `sched.offcpu`, `cpu.oncpu_time`, `sched.futex`, `mem.rss`, `io.syscall`, `net.socket` |
