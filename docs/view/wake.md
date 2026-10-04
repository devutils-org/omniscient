# The `wake` view: who woke whom

Stream wake of the nineteenth wave. One of the nine experiments of `omn top`;
this page says what it draws, what it showed on the fixture runs, and what it
cannot show.

```console
omn top --replay tests/view_fixtures/http-server --view wake
omn top -- ./prog                      # then 8, or → until "wake"
```

## What it draws

From `sched.wakeup` alone (one event per wakeup: the wakee, the waker -- a task
or an interrupt -- and when), and nothing else:

- **A headline**: every wakeup of the run, and how many came from a task, a
  hard interrupt, a soft interrupt; the source's trust badges beside it.
- **A grid of the woken threads**, as many columns as the width holds: each
  thread's wakeups and what mostly woke it. The target's threads first; the
  threads outside it that the target woke are dimmed and said so. `↑`/`↓`
  choose one; the grid pages around the chosen thread.
- **The chosen thread's wake tree.** Each wakeup is followed back in time: the
  thread was woken by W1; W1 had itself been woken a moment before by W2; and
  so on, until the chain reaches an interrupt, a thread already on it (`↺`, a
  request and its reply), a waker whose own wakeup the run did not see, or the
  depth (`+`/`-`, 6 by default). The chains are folded into a tree, as a
  profiler folds stacks, and each branch says:

  | Column | What |
  |---|---|
  | wakes, share | how many of the thread's wakeups came this way |
  | quiet | median time the thread this link woke had shown **no sign of life** in the run -- no wakeup received or issued -- before it: at most how long it was blocked |
  | hold, max hold | median and worst time the **waker** held the chain: from its own wakeup to the one it issued |
  | when in the run | the branch's wakeups across the run, one cell per slice, scaled to the thread's busiest slice |

- **The longest chains**, one wakeup each, link by link with each hold: the
  single worst paths the tree's medians fold away. Wakeups one earlier wakeup
  led to, along the same links, are one line with their count.
- **What the thread woke in turn.**

`enter` narrows every view to the thread (pid and tid); `l` opens it in the
lanes view. A focus set elsewhere is honoured: a tid chooses the thread, a
pid keeps only its threads in the grid.

Trust is the frame's: `sched.wakeup` is a `low` determinism source, so every
figure is dimmed, and the rounds that came late are badged (a late read is a
read: the timestamps are the kernel's). A live run draws what it has so far,
badged `live`; a run without the source says so in place of the tree, and a
refused one draws the refusal.

## What it showed on the fixtures

Snapshots in `docs/view/gallery/wake--*.txt` (and `.ansi`, `cat` them).

**`http-server`** -- the standard library's HTTP server under the case's
client, 25 listings of a 15 000-entry directory
(`wake--http-server--main.txt`, `wake--http-server--handler.txt`):

- The server's main thread was woken 18 times. 10 came from its handler
  thread `Thread-1`, which had **held the chain 273 ms at the median, 501 ms
  at worst** -- the time a listing takes to build -- and the longest chains
  spell it: `<- Thread-1 501ms <- python3 39.2µs ↺`, the main thread handing a
  connection over in 39 µs and getting it back half a second later.
- The main thread's five hard-interrupt wakeups each came after a **quiet of
  501 ms**: the accept loop's poll timeout (`serve_forever`'s default
  `poll_interval` is 0.5 s) -- reasoned from the figure; the view only says a
  timer-shaped interrupt woke a thread that had slept half a second.
- The client was woken 176 times, 86 % from a **soft interrupt**: loopback
  delivery of the server's replies. That is the source's own caveat -- a
  softirq served in its sender's `local_bh_enable` has the interrupt as its
  waker -- and the tree says it in place ("a device, or a loopback send")
  rather than naming a sender it does not know.

**`sqlite-commits`** -- 40 commits, rollback journal, full sync:

- `sqlite3` was woken 1 019 times: 54 % by a **hard interrupt** (the disk's
  completions; quiet 378 µs at the median, at most what each wait on the
  device took), 12 % by soft interrupts, and the rest by a dozen threads
  outside the target that the run cannot name -- `sched.wakeup` on a launched
  target sees a non-target thread only as the waker or the wakee of the
  target, never its own wakeups, so those chains stop at the first link and
  say so ("outside the target: stops").
- It woke a thread named `gmain` outside the target 204 times, and the
  per-cpu block workers dozens of times each: what a commit sets going.
- The "when" strips show the hard-interrupt wakeups spread over the whole run,
  as one fsync per commit should.

**`cpython-hot`** -- a cpu-bound interpreter: one wakeup in four seconds. The
view is nearly empty, which is the finding: the program's time is not in the
wake chain.

**`machine`** draws "did not trace sched.wakeup": the overview fixture asks
only the system view's sources. **`sqlite-latency`** and
**`vertical-requests`** are the same: their sessions do not ask for it.

## What it does not show

- **Chains through threads outside the target.** Their own wakeups are not
  traced on a pid or launch target. A session that asks `sched.wakeup` on the
  machine (`omn top -f` with a system-scope request) gives the view every
  link; the view draws it unchanged, since it reads every result of the
  source. It is not the default because a machine-wide trace of every wakeup
  is large (the fixture notes measured every view's needs on the machine at
  60 MB for eight seconds).
- **Exact blocked time.** `quiet` is an upper bound: the thread may have run
  after its last wakeup received or issued. The exact off-cpu time, and the
  stack it blocked in, are the off-cpu sources', drawn by flame and lanes.
- **Who sent a loopback softirq**, as above; nor which device a hard interrupt
  came from (the source does not record the irq number).
- **The waker's comm when the run never saw it as a wakee**: it is drawn as
  `tid N`.

**Verdict:** it shows what no other view does -- who handed a thread each
wakeup and how long every hand held it, e.g. the HTTP server's handler holding
the main thread's chain 273 ms per listing and the poll timer waking it after
501 ms of quiet -- but not chains through threads outside the target, exact
blocked time, or the sender behind a loopback softirq.
