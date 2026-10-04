# The `timebar` view: where the wall-clock went

Stream timebar of the nineteenth wave. One bar per thread or task, split into
the states its time went to -- on cpu, the run queue, a lock, a disk, a child,
the network, a sleep -- with the first suspect named on it, by the rule `omn
latency` names it with. Tab 5 of `omn top`; the code is
`view/src/views/timebar.rs`.

```console
omn top --replay tests/view_fixtures/sqlite-latency --view timebar
omn latency --objective latency --dump-session -- PROG > s.yml && omn top -f s.yml   # then 5
```

## What is on the screen

```
 where-the-time-went  one bar per task, whole: the program's life, gate to reap

 ▶ first suspect  task 2759527: blocked 242ms (85.3% of its wall clock)
   off cpu, blocked -- not split by task state here, so a disk and a sleep are not told apart
   48 % of it from one stack, leaf first: fdatasync <- sqlite3+0x19a6a9 <- sqlite3+0x6eabb <- ...
   decisive: ahead of the next state (43.1ms) and of what no row read (1.00ms) -- omn latency's rule

 ▸ task 2759527        ├█████████▚▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓ blocked 85.3% ▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓┤    284ms  ▶ blocked 85.3%
                      0 ───────────────────────────┼ 144ms ───────────────────────────┤ 287ms
```

From the top:

- **The heading**: where the bars come from (a decomposition by its id, or
  `the view's reading`), what one bar is, what its whole is, and which of
  several decompositions this is (`d` steps through them).
- **The first suspect** of the chosen bar, in four lines: the state and its
  share of the whole, what a task limited by it is doing (in `omn latency`'s
  words), what names it -- the stack it blocked in, the thread that held the
  lock, the device, the most-sampled function -- and why it is decisive: by
  how much it is ahead of the next state and of what no row read. A bar whose
  largest state is not decisive says `no single suspect`, and why, in the
  triage's own sentence.
- **The bars**, one per thread or task, every one on one time scale (`s`
  switches to each bar on its own whole). Each state fills its share with its
  own cell -- `█` on cpu, `▚` run queue, `▞` lock, `▓` I/O or blocked, `▙`
  child, `▄` network, `░` sleep -- so a bar reads in plain text as it does in
  colour, and a share wide enough carries its name and percentage inside it.
  `▒` is a bracket (below), `·` time no row read. The bar's suspect is at its
  end. A thread whose states add up to under 1 % of the window *even at
  their most* -- a bracket counted at its upper end -- is folded into one
  line; a busy worker whose bracket's lower end is zero is never folded.
- **The chosen bar's table**: the whole, every state with its source, amount,
  share, resolution, readings and the source's trust badges, what names it
  (`↳`), and the residue -- the whole less the rows, signed, said to be
  within its resolution or not. Nothing is rounded into another row. In the
  view's own reading, where on cpu is derived from what the other rows leave
  of the window, the rows add up to the whole by construction: the residue
  is then said to be not measured rather than printed as a zero that cannot
  fail.
- **Notes**: where the bars came from, what a bracket is, what the whole
  means, and every trust mark of the bar in a sentence.

Keys: `↑`/`↓` choose a bar, `d` the next decomposition, `s` one time scale or
each bar to its own whole, `enter` opens the chosen thread in `lanes` (or the
chosen request in `request`), narrowed to it. A focus set elsewhere narrows
the bars to its thread, process or request.

## Where the bars come from

**A decomposition in the run.** `omn latency`'s session carries
`where-the-time-went` (one table per task, whole = the program's life); the
vertical case's session carries `one-request` and `block-io` (one table per
request). Each table is a bar, its rows exactly as the join engine wrote them:
the view adds nothing to a row and removes nothing. Two things are read beside
the rows, as `omn latency` reads them:

- the `blocked` row is split by the task state each block recorded --
  `uninterruptible` drawn as I/O, `sleeping` as a sleep, a sleep `sched.futex`
  covers half of as a lock, a sleep whose stack waits for a child as a child
  -- **when the run holds `sched.offcpu`'s events**. A recording that kept its
  stacks only (the `sqlite-latency` fixture) cannot be split, and the bar and
  its note say so: it is drawn `blocked`, named by its heaviest stack.
- the on-cpu row is named by the profile's most-sampled leaf, and says when
  the samples are spread over many functions rather than pretending one
  dominates.

**The view's own reading**, when no decomposition splits the run by task --
every `omn top` run without a session, and any live run until it ends (the
join engine runs at the end). Per thread seen by a sched source, over the
run's window:

| State | From |
|---|---|
| blocked, by task state | `sched.offcpu`'s events (`total_ns` less `queued_ns`, as the triage) |
| run queue | `sched.runqlat`'s events, or its per-task summary |
| lock | `sched.futex`'s per-task waits, when they cover half a sleep |
| on cpu | `cpu.oncpu_time` |

`cpu.oncpu_time` is served by `fs:proc` on this machine: a **per-process**
total, keyed on the process id, to a tick. A thread's share of it is not
known, and is not invented: it is a bracket, drawn solid to the least it can
be -- the process's total, less a tick, less what every other thread of the
process could have run -- and shaded `▒` to the most -- what the thread's own
other states leave of the window. A single-threaded process's bracket is its
cpu time, with the window's rest (the time before it executed) shaded; the
http server's handler thread is bracketed 2.60 s to 2.83 s, its main thread
0 to 87 ms. A thread whose process `cpu.oncpu_time` did not measure (a
process the target started) has its on-cpu share unread, and its bar shows
the remainder as `· not read`.

The heading says which of the two a screen is, and the reading's note says
that its suspect follows `omn latency`'s rule without being its answer, with
the two commands that get the answer.

## The rule

The first suspect is `omn latency`'s (`src/triage/latency.rs`, `latency()`),
restated because the view's crate cannot depend on `omn`'s: the largest state
read is named when it is larger than the next one -- at the most that one
could be, a bracket's upper end -- and larger than what no row read: the
whole less what was read, plus the residue's resolution, when a row is
missing; the rows' own resolutions when every row was read. Otherwise no state
is named, and the reason is the triage's sentence. A lossy source's amount is
a lower bound, drawn `≥`; a bracket's too.

The restatement is held to the triage by `tests/view_timebar.rs`, which runs
`omn::triage::latency::read` and `conclude` on every fixture carrying
`where-the-time-went` and asserts that the timebar names the same state, or
none when the triage names none. The one deliberate difference -- the next
state at its most rather than the second-largest amount -- changes nothing on
a decomposition, whose rows carry no bracket.

Not restated: following the main thread into the child or the thread that ran
during its wait (`omn latency` does; the view shows every thread side by
side instead), the second pass with `sched.mutex`, and the tail objective.

## On the fixtures

Every fixture's snapshot is `docs/view/gallery/timebar--<fixture>.txt` (and
`.ansi`, `cat` it). Four more states of the view sit beside them:
`timebar--sqlite-commits-live` (half way through the run, from its events
alone), `timebar--http-server-handler` (the handler thread chosen),
`timebar--vertical-requests-own-scale` and `timebar--vertical-requests-block-io`.

| Fixture | What the bars say |
|---|---|
| `sqlite-latency` | `omn latency`'s own table: blocked 242 ms of 284 ms (85.3 %), from the commit's `fdatasync`; on cpu 15.2 % at a 1 ms sampling resolution; residue −3.26 ms within its ±9.54 ms |
| `vertical-requests` | each request named by the cause the server injected: a001 a lock (99.8 %, the holder's tid named), a002 the disk (81.8 %), a003 a lock then the disk, a004 the network on the way in (a client that pauses), a005 the network on the way out (a client that reads slowly); `d` shows the block I/O each one waited for, on `nvme0n1p6` |
| `http-server` | the server's main thread asleep in `select` (96.9 %), its handler thread on cpu for at least 2.60 s (the most-sampled leaf `link_path_walk`: the directory listing), the client asleep waiting for answers; `ldconfig` folded |
| `sqlite-commits` | on cpu ≥ 86.5 % (the tenth of a second of work between commits), the disk 8.2 % from `fdatasync`, run queue 0.13 % |
| `cpython-hot`, `cpython-hot-heavier` | on cpu ≥ 95.4 % and ≥ 96.7 %, never blocked |
| `machine` | nothing to split: no target, no sched source; the screen says what is needed and the commands that give a decomposition |
| live, half way | on cpu unread until the run ends (the poll is said at the end), so no state is named: `blocked: uninterruptible` 207 ms could still be overtaken by 2.27 s no row read |

## What it does not do

- It does not place states along time: a bar is a sum, not a timeline.
  Where in the run a thread blocked is the `lanes` view's.
- It cannot split a `blocked` row from a recording that kept only the
  stacks. `omn latency` splits it from events it holds in memory and never
  writes; `omn top -f` on its session writes what the session asked, which is
  stacks.
- It does not give a multi-threaded process's cpu to its threads exactly on
  this build: the catalog's per-thread backends of `cpu.oncpu_time`
  (`bpf:raw_tp`, `bpf:tp`) have no collector (`src/collect/cpu/mod.rs`
  registers `fs:proc` only), so the chain falls to the per-process total. The
  bracket is what can be said; on the http server it is 8 % wide.
- It does not follow a wait into the task that ran during it, as `omn
  latency` does: it draws both tasks and lets the eye do it.

**Verdict**: it shows, in one screen, every thread's wall clock split into
the states its time went to with the suspect `omn latency` would name and the
evidence that names it -- the only view that says *why* a thread was slow
rather than *where*, and that draws an unattributable cpu share as the bracket
it is; it does not show *when* (no time axis inside a bar), and a recording
without `sched.offcpu`'s events cannot tell a disk from a sleep.
