# lanes -- threads side by side

Stream lanes of the nineteenth wave. One lane per thread and one per GPU
stream, along one time axis, so a kernel and the host thread that waited for
it are drawn one above the other. `view/src/views/lanes.rs`; its snapshots are
`docs/view/gallery/lanes--*`.

```console
omn top --replay docs/view/gallery/lanes-gpu --view lanes   # the recording below
omn top --view lanes -- ./prog                              # live
cat docs/view/gallery/lanes--gpu-feeder.ansi                # the snapshot, in colour
```

## What is on screen

```
status   each source: its events and its trust badges (late, low det, ±time error)
ruler    time from the run's opening; "27.4ms a cell" says the resolution
lanes    gpu0 s13 spin         │ ▁ ▁ ▁▃████▇██▆██▆██   │ busy 48.1%
         feeder        3065042 │██▃ ▁▁ ▁▁▁ ▁▁  ▁  ▁    │awake 10.9%
                       off cpu │▁▁▆█████████████████   │ blkd 85.3%
         logger        3065051 │  ▁───────▁──────▁──   │awake 0.05%
rule     ▲ the cursor's instant
detail   the selected lane at the cursor, what every other lane was doing then,
         and where the thread was off a cpu over the whole run, by stack
legend
```

- **A thread** that was awake for a hundredth of the span or more gets two
  rows: what share of each cell it was **awake** (green, a bar's height), and
  what share it was **blocked** (blue when sleeping, yellow when
  uninterruptible -- usually a disk). A thread that slept throughout gets one
  row: a line where it was blocked, a bar where it woke. Two rows go to as
  many active threads as the room allows, the main thread first.
- **A GPU stream** gets one row: the share of each cell a kernel (magenta) or a
  copy (cyan) ran on it. The streams of a process are drawn right above its
  main thread, the thread that most often launches on them.
- **The detail** names the state the cursor is in, from when to when, the cpu
  the thread left, who woke it (a thread, or an interrupt), how long it then
  queued for a cpu, the stack it blocked in, and, on one line, what every other
  lane was doing at that instant. Under it, the thread's off-cpu time over the
  whole run, by state and stack, from `sched.offcpu`'s own totals.
- **Our own threads** in the target (the CUPTI library's `omn-cupti-flush`)
  are drawn dim and italic, name and cells: they are the tool's cost, not the
  program.

Keys: `↑`/`↓` (`j`/`k`) a lane, `h`/`l` the cursor by a cell, `H`/`L` by ten,
`[`/`]` to the previous or next change of the selected lane, `z`/`Z` zoom in
and out on the cursor, `w` to the thread that woke the block under the cursor
(the cursor lands just before the wakeup), `enter` narrows every view to the
selected thread or process. The cursor opens on the run's most telling
moment: the main thread's longest wait for the GPU -- a block that ended as a
kernel or a copy did, the activity running through four fifths of it -- or,
where no GPU ran, the main thread's longest block.

## Where each glyph comes from, and how far it holds

A thread's lane is rebuilt from two exact series of instants, both on
`CLOCK_MONOTONIC`, with a zero time error:

| Instants | Source | What it says |
|---|---|---|
| switch-out | `sched.switches` (`pmu:sw`) | the thread left a cpu, and which |
| wakeup | `sched.wakeup` (`bpf:raw_tp`) | something woke it: a thread, or an interrupt |

- A switch-out followed by a wakeup is a **block**, from one to the other.
- From a wakeup to the next switch-out the thread was **awake**: on a cpu or
  queued for one. `pmu:sw` has no switch-in, so the two cannot be told apart
  per instant; a block whose `sched.offcpu` row is that block alone says how
  long it then queued (`then queued 18.9µs for a cpu`).
- Two switch-outs with no wakeup between are a **preemption**: the thread was
  runnable through it, and is drawn awake.
- Before a thread's first instant and after its last nothing is drawn.
- In a run without `sched.wakeup` the view says so, draws the time between two
  switch-outs as `┄` (blocked or not, it cannot tell) and the awake share as
  `?`.

`sched.offcpu` does not give blocks one by one: an event is a row, one per
(thread, state, stack, log2 bucket) per drain window, stamped at the drain
(`src/collect/sched/offcpu.rs`). A block takes its state and stack from its
thread's row whose window holds its end and whose mean block is nearest in
size. The detail says which: `its sched.offcpu row is this block alone` (one
block, within an eighth of a doubling of the reconstructed length, and no
other row of the window that could hold it), `state, stack: the nearest in
size of N sched.offcpu rows of its window that could hold it` (another
one-block row within an eighth of a doubling, or a row of several blocks whose
mean is within a doubling: which row is this block cannot be told, and no
queueing is said), or `state, stack: its window's sched.offcpu row nearest in
size`. On the recording the
stacks under the main thread's waits are the driver's `cuCtxSynchronize` and
CPython's `_PySemaphore_Wait` (the queue).

A GPU lane is `gpu.kernel`'s activities, each a start converted from the
device's clock (to within ±295 ns on the recording) and a duration. They are
keyed on the process, not on the thread that launched them -- CUPTI's
activity records carry a correlation id, and the launch's API record is not
collected -- so a kernel lines up with its host thread **in time**, never by
attribution: the view never says which thread launched a kernel.

Every glyph is drawn in its source's trust style: an awake bar in the
instants' (`sched.switches`, `sched.wakeup`), a block's cells and its state
in the instants' and `sched.offcpu`'s joined (the instants place it,
`sched.offcpu` colours it), the queueing, the stack and the off-cpu totals in
`sched.offcpu`'s (its badges beside the totals' heading), a GPU activity and
its duration in `gpu.kernel`'s. On this machine that dims every lane: `sched.*` are `low` determinism, and `gpu.kernel` carries a time
error, so the dimming says nothing that tells one lane from another here (see
"Needs from another stream" in `docs/fixes/w19-lanes.md`).

## The recording it was judged on

The shared fixtures were recorded before `gpu.kernel` had a collector, so the
GPU lanes there are its refusal. The view is judged on its own recording,
`docs/view/gallery/lanes-gpu/` (only the four sources it reads, 1.4 MB),
made by `omn top` itself from `docs/view/gallery/lanes-gpu/workload.py`: a
Python program in the shape of a training loop -- a `loader` thread prepares
batches on the cpu, a `feeder` copies each to the device, launches a `spin`
kernel on one stream and a `copy` on another, and waits in
`cuCtxSynchronize` (a context made with `CU_CTX_SCHED_BLOCKING_SYNC`, so the
wait is a block), and a `logger` wakes every 200 ms. The first half's batches
are dear to prepare and cheap to compute; the second half's the reverse.

```console
make
target/debug/omn top --headless -d 20s -o DIR -- python3 docs/view/gallery/lanes-gpu/workload.py
# then keep sched.switches, sched.wakeup, sched.offcpu and gpu.kernel, and rewrite the paths
```

What the snapshots show (`lanes--gpu-feeder`, and `lanes--gpu-feeder-zoom`,
the same at 3.4 ms a cell after `zzz`):

- **The two regimes, without being told.** For the first second the `loader`
  is awake and the `feeder` is blocked on the queue; from 1.1 s the `spin`
  lane fills, the `feeder` blocks in `cuCtxSynchronize` for each kernel, and
  the `loader` is blocked too -- on a full queue. Which side is the
  bottleneck changes half way, and it reads off the screen.
- **The wait, the kernel and the wakeup, lined up.** At the cursor the
  `feeder` is blocked 132 ms, from 1.62 s to 1.75 s; `spin` on s13 runs 132
  ms under it; the `feeder` is woken by `cuda-EvtHandlr`, the driver's thread
  that a hardware interrupt woke 27 µs earlier, as the kernel ended (`w` on
  the block goes there). Zoomed, each kernel's end, the `feeder`'s brief
  wake, the copy back, the `loader`'s wake and the next kernel's start are
  one column apart each.
- **The program's threads that it did not write.** The driver's six threads
  (four named `feeder`, after the thread that started them, `cuda-EvtHandlr`
  and `cuda00001400006`) and our own flush thread are there, with what they
  were awake: 0.66 % for the event handler, 0.11 % for our flush thread.

On the shared fixtures: `http-server` shows the server's main thread asleep in
`select` while its connection thread works in bursts; `sqlite-commits` the
one `sqlite3` thread awake 92 % with a sliver of uninterruptible `fdatasync`
in every cell, all of its 410 ms off a cpu under `fdatasync`; `cpython-hot`
one thread awake throughout, 102 switch-outs and one wakeup; `machine`, `sqlite-latency`
and `vertical-requests` have no thread events with times (the last two are
sessions that ask summaries and stacks), and say so.

## What it does not show

- **On a cpu versus queued for one**, instant by instant: the free switch
  counter has no switch-in. Per block, the queueing is said when the block's
  `sched.offcpu` row is that block alone.
- **A queueing of zero from one the kernel did not stamp**: `sched.offcpu`
  writes an unstamped `queued_ns` as zero (its degradation says how many), so
  a zero is drawn `queued for a cpu: 0, or not stamped`, never as a measured
  zero. On the recording all but one of the main thread's rows carry a zero
  `queued_ns`.
- **Why a thread was preempted**, or which thread ran instead: `pmu:sw` has no
  second party.
- **Which thread launched a kernel**: see above. The lining up is in time.
- **What happened inside a cell** at the whole run's zoom: a 4 ms
  `fdatasync` in a 60 ms cell is a one-eighth bar. `z` goes down to the
  instants.
- **More threads than rows**: past the screen the lanes scroll; a server with
  hundreds of threads is a list to scroll, not a picture.
- `sched.wakeup` is read but not yet declared in the view's needs (the table
  in `README.md` is core's): a session that names only the lanes' needs would
  draw `┄`. `omn top` live asks every view's needs, the wake view's included,
  so it always has it.

**Verdict:** lanes shows what no other view does -- which thread waited for
what, when, and who ended the wait, a host thread's block lined up under the
kernel it waited for and the regime change of a pipeline read straight off
the screen; it does not say what ran instead of a preempted thread, whether
an awake thread was on a cpu or queued, or which thread launched a kernel, and
at the whole run's zoom it shows shares, not instants.
