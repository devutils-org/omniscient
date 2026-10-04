# The gallery: nine views to judge

Every view of `omn top` is an experiment (`docs/briefs/wave-19-gpu-view.md`):
built in the nineteenth wave, drawn on the same recorded runs, and kept or
dropped on what it shows on screen. This page is where that judgement is
made: one row per view, the snapshots it is judged on, how to see it live and
replayed, and its stream's verdict -- what it shows that nothing else does,
and what it does not. The last column is the user's.

Each snapshot is a pair: `<name>.txt`, the characters, and `<name>.ansi`, the
same screen with its colours and its dimming (`cat` it in a terminal; the trust
style is only in the `.ansi`). Both are drawn at 120x40 unless the name says
otherwise, and both are held by a test: `tests/view_snapshots.rs` for the
`<view>--<fixture>` set, the view's own unit tests in
`view/src/views/<view>.rs` for the rest.

## Seeing them

```console
make                                                   # the capabilities, for a live run
omn top --gallery tests/view_fixtures/http-server      # every view on one run: space steps, 1-9 jump
omn top --replay tests/view_fixtures/http-server --view wake   # one view on one run
omn top --gallery tests/view_fixtures/sqlite-commits --headless --size 120x40   # every view as text
omn top -- python3 cases/cpython-interpreter/hot.py    # live; 1-9 or tab between views, ? for help, q quits
```

`omn top` needs a terminal: in a shell without one it is refused at once and
names `--headless`. The seven common recordings are `tests/view_fixtures/`
(`docs/view/README.md`, "The fixtures", says what each holds): `machine`,
`cpython-hot`, `cpython-hot-heavier`, `http-server`, `sqlite-commits`,
`sqlite-latency`, `vertical-requests`. Two views carry a recording of their
own here, because no common fixture held what they draw:
`lanes-gpu/` (a host program feeding the GPU) and `flows-run/` (a file
server). Every snapshot of every view on every common fixture exists; the
table names the ones worth opening first.

## The views

| # | View | Snapshots to open first | Live | Replayed | Verdict (its stream's) | Keep? |
|---|---|---|---|---|---|---|
| 1 | **system** -- the overview (`docs/view/system.md`) | `system--machine`, `system--sqlite-commits`, `system--http-server`; `frame--narrow` (60x20, sections left out and said) | `omn top`, or `omn top -- ./prog` | `omn top --replay tests/view_fixtures/machine` | It shows every resource of the machine on one time axis, drawn in the trust style with the GPU's one-second sampling error as a ruler at its real width and each collector's price beside it, which no system monitor does; it does not show per-cpu load, processes other than the target's, or, once a run has ended, the splits (by state, in/out, read/written) that only live events carry. | |
| 2 | **flame** -- a flamegraph in the terminal (`docs/view/flame.md`) | `flame--cpython-hot`, `flame--sqlite-commits` (all off cpu in `fdatasync`), `flame-against--cpython-hot--cpython-hot-heavier` and `flame-against-shape--…` (38 of 378 functions moved, every one up; in shape, none), `flame-threads--http-server`, `flame-zoomed--http-server`, `flame-search--http-server`, `flame-narrow--http-server` (80x24), `flame-live--sqlite-commits` | `omn top --view flame -- python3 cases/cpython-interpreter/hot.py` | `omn top --replay tests/view_fixtures/cpython-hot --view flame`; two runs: `omn top --replay tests/view_fixtures/cpython-hot --against tests/view_fixtures/cpython-hot-heavier --view flame` | The only place in `omn` that shows a profile's shape -- on-cpu and off-cpu side by side, per thread, and two runs' difference judged against the noise both absolutely and in shape -- but it draws the stacks it is given, so an interpreter's own functions and a frameless leaf's callers stay invisible. | |
| 3 | **ledger** -- what a process costs (`docs/view/ledger.md`) | `ledger--http-server`, `ledger--focus--http-server` (one thread, the I/O line open), `ledger--against--cpython-hot-heavier`, `ledger--sqlite-commits` | `omn top --view ledger -- ./prog` | `omn top --replay tests/view_fixtures/http-server --view ledger`; `--against tests/view_fixtures/cpython-hot-heavier` on `cpython-hot` | The ledger shows what nothing else does -- one process's cost across cpu, memory, I/O, network volume, GPU and energy on one screen (the retransmissions only the machine's), each line beside what it cost `omn` to know and how far it can be trusted, and two runs compared line for line; it does not show where inside the process the cost went, nor any figure its sources do not carry (no byte counts from faults, no measured per-process energy). | |
| 4 | **request** -- what one request costs (`docs/view/request.md`) | `request--vertical-requests`, then `request--vertical-requests--a001` to `--a005` (one per request: lock, disk, lock and disk, a client that paused, a slow reader); `request--http-server` (the refusal, in place) | `omn top -f examples/vertical-one-request.yml --view request` | `omn top --replay tests/view_fixtures/vertical-requests --view request` | It is the only screen that names, per request and without being told, why that request was slow -- the lock and who held it, the disk with each block request on its own time axis, the client that paused or read slowly -- and what set it apart from its siblings; it does not show the order of states inside a request (only their sums are recorded), nor anything for a program that does not declare its requests. | |
| 5 | **timebar** -- where the wall-clock went (`docs/view/timebar.md`) | `timebar--sqlite-latency` (`omn latency`'s own session: `blocked` named by `fdatasync`), `timebar--vertical-requests`, `timebar--vertical-requests-own-scale`, `timebar--vertical-requests-block-io`, `timebar--http-server-handler`, `timebar--sqlite-commits-live` | `omn latency --objective latency --dump-session -- ./prog > s.yml`, then `omn top -f s.yml --view timebar` | `omn top --replay tests/view_fixtures/sqlite-latency --view timebar` | It shows, in one screen, every thread's wall clock split into the states its time went to with the suspect `omn latency` would name and the evidence that names it -- the only view that says *why* a thread was slow rather than *where*, and that draws an unattributable cpu share as the bracket it is; it does not show *when* (no time axis inside a bar), and a recording without `sched.offcpu`'s events cannot tell a disk from a sleep. | |
| 6 | **heatmap** -- latency over time (`docs/view/heatmap.md`) | `heatmap--sqlite-commits` (three regimes pulsing once per commit), `heatmap--sqlite-commits--zoom-x8`, `heatmap--http-server--sched.runqlat`, `heatmap--vertical-requests`, `heatmap--sqlite-commits--live-half-way`; `../drawings/heatmap--gpu-phases.txt` (a GPU run, hand-kept, not held by a test) | `omn top --view heatmap -- ./prog` (`s` steps the source) | `omn top --replay tests/view_fixtures/sqlite-commits --view heatmap` | It shows what no histogram and no mean timeline can -- that a latency's modes are separate events in time (sqlite's slow writes once per commit, a GPU program's phases, a server's first requests) and which one measurement the spike was -- but not why a band sits where it does, and for the kernel-bucketed sources its time is only as fine as their ~100 ms drain window. | |
| 7 | **lanes** -- threads side by side (`docs/view/lanes.md`) | `lanes--gpu-feeder` (a host thread's block under the kernel it waited for, from `lanes-gpu/`), `lanes--gpu-feeder-zoom` (3.4 ms a cell), `lanes--http-server`, `lanes--sqlite-commits` | `omn top --view lanes -- python3 docs/view/gallery/lanes-gpu/workload.py` | `omn top --replay docs/view/gallery/lanes-gpu --view lanes` | Lanes shows what no other view does -- which thread waited for what, when, and who ended the wait, a host thread's block lined up under the kernel it waited for and the regime change of a pipeline read straight off the screen; it does not say what ran instead of a preempted thread, whether an awake thread was on a cpu or queued, or which thread launched a kernel, and at the whole run's zoom it shows shares, not instants. | |
| 8 | **wake** -- who woke whom (`docs/view/wake.md`) | `wake--http-server--main` (the handler holding the main thread's chain), `wake--http-server--handler`, `wake--http-server`, `wake--sqlite-commits` (54 % by hardirq) | `omn top --view wake -- ./prog` | `omn top --replay tests/view_fixtures/http-server --view wake` | It shows what no other view does -- who handed a thread each wakeup and how long every hand held it, e.g. the HTTP server's handler holding the main thread's chain 273 ms per listing and the poll timer waking it after 501 ms of quiet -- but not chains through threads outside the target, exact blocked time, or the sender behind a loopback softirq. | |
| 9 | **flows** -- where the bytes went (`docs/view/flows.md`) | `flows--file-server` (from `flows-run/`: every file's bytes exact, loopback 2.00x the socket, the page cache 0.13x), `flows--http-server`, `flows--sqlite-commits` (no bytes per file on the default request, said) | `omn top --view flows -- ./prog` (bytes per file need `io.fs`: `docs/view/flows.md`) | `omn top --replay docs/view/gallery/flows-run/data --view flows` | Flows is the only view that puts the target's own bytes and the machine's on one scale across the kernel layer between them, so the page cache's absorption (0.13x) and loopback's double count (2.00x) are seen as a change of height rather than computed; it does not say which socket used which interface or which file's bytes reached which device -- nothing measures those edges -- and on the default request it has no bytes per file at all. | |

## The frame's own

`frame--help` (the keys and the trust legend), `frame--gallery` and
`frame--live` (both draw the heatmap, so a heatmap change re-blesses them),
`frame--refusal` and `frame--narrow`: the parts every view shares, by stream
core (`docs/view/README.md`).

## What every view shares, and what to look for

- **The trust style**: a figure with losses, a time error or low determinism
  is dimmed, with its badge beside it; a refused source draws its reason where
  its panel would be. Most of the catalog's traced sources are `low`
  determinism, so most figures on these screens are dimmed; lanes found the
  rule tells no lane from another on a timeline, and a scale-aware rule is
  open (`docs/open-findings.md`, the nineteenth wave, stream lanes).
- **The cost corner**: the view's own cpu beside the collectors' and the
  target's, as shares of one cpu.
- **Not yet seen on a real terminal by anyone but stream core**: every view
  stream judged its view on these snapshots and on `--headless`, because an
  agent shell has no terminal. The live commands above are the look still
  owed.
- **GPU**: the seven common recordings predate `gpu.kernel`'s collector and
  draw its refusal; only `lanes-gpu/` and the heatmap's hand-kept drawing hold
  GPU kernels. A common GPU fixture is in `STATE.md`'s "What remains".
