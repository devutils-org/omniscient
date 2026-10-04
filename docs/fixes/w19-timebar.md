# The nineteenth wave, stream timebar

The `timebar` view of `omn top`: where the wall-clock went, one bar per
thread or task, by state, with the first suspect named on it -- the same one
`omn latency` names, by its rule. Branch `feat/w19-timebar`. Files touched:
`view/src/views/timebar.rs`, its snapshots `docs/view/gallery/timebar--*`,
`docs/view/timebar.md`, and this handoff; in the review round also
`tests/view_timebar.rs` (new: the parity with `omn latency`) and the
timebar's rows of `docs/view/README.md`'s table of needs -- both core's to
own from here.

## What landed

- **The view** (`view/src/views/timebar.rs`). Each table of every
  decomposition in the run is a bar, its rows exactly as written, its residue
  signed beside it; `d` steps through decompositions. When no decomposition
  splits the run by task (every plain `omn top` run, and every live run
  before the join engine runs at its end), the view reads the traced sources
  per thread over the run's window and says on screen that it is its own
  reading, with the commands that give `omn latency`'s.
- **The first suspect, by `omn latency`'s rule** (`decide`): the largest
  state read, named when it is ahead of the next one (at its most) and of
  what no row read (the whole's remainder plus the residue's resolution when a
  row is missing; the rows' own resolutions otherwise). Otherwise `no single
  suspect`, with the triage's sentence. Each suspect is named by what the run
  holds: the heaviest blocking stack, the lock holder a join named, the
  device, the most-sampled leaf -- and a leaf that has under a fifth of the
  samples is said to be spread rather than presented as the culprit.
- **`blocked` split as the triage splits it** when the run holds
  `sched.offcpu`'s events: `uninterruptible` as I/O, `sleeping` as a sleep, a
  sleep `sched.futex` covers half of as a lock, a sleep whose blocking stack
  is a wait for a child as a child. The split partitions the events it is
  made of (a test); where their sum is not the decomposition's row, the
  residue carries the difference and a note says so, so the bar and its
  residue still add up to the whole.
- **On cpu from a per-process total, as a bracket.** `cpu.oncpu_time` is
  `fs:proc` here: one total per process, keyed on the process id. A thread's
  share is drawn solid to its least (the process's total less a tick less
  what the process's other threads could have run) and shaded `▒` to its most
  (what its own other states leave of the window), never apportioned.
- **The screen**: a heading (where the bars come from, what a whole is), the
  chosen bar's suspect in four lines, the bars on one time scale (`s`: each to
  its own whole) with a distinct cell per state so the text snapshot reads
  like the colour one, an axis, a legend of the states on screen, the chosen
  bar's table (whole, states with source, amount, share, resolution,
  readings, trust badges, what names each; residue), and the notes and trust
  sentences. Threads under 1 % of the window even at their most (a bracket
  at its upper end) are folded into one line. `↑↓`
  choose, `enter` opens the thread in `lanes` or the request in `request`,
  narrowed; a focus narrows the bars. A run with nothing to split (the
  `machine` fixture) says so, what each need holds, and how to get a
  decomposition.
- **Snapshots**: the seven fixtures, plus four states of the view --
  `sqlite-commits-live` (half way, from events alone), `http-server-handler`
  (the handler thread chosen), `vertical-requests-own-scale`,
  `vertical-requests-block-io`. Thirteen unit tests in the module.
- **`docs/view/timebar.md`**: what is on the screen, where the bars come
  from, the rule, what each fixture says, what the view does not do, and the
  verdict.

## Shown versus reasoned

Shown, by tests on the committed recordings:

- On `sqlite-latency` (`omn latency`'s own session) the suspect is `blocked`,
  85.3 % of the program's life, named by the `fdatasync` stack; the residue
  is the table's, −3.26 ms, within its ±9.54 ms.
- On `vertical-requests` every request's suspect is the cause the server
  injected: a001 and a003 a lock (a001's holder tid named), a002 the disk,
  a004 and a005 the network.
- On `http-server` the handler thread's on-cpu bracket is a bracket (least ≤
  most ≤ whole) holding more than half of the window, the server's main thread
  is named asleep, and no thread's states add up to more than its whole.
- On `cpython-hot` the suspect is on cpu; on `sqlite-commits` the split of
  `blocked` adds up to the events' sum and holds I/O.
- A live run (`feed::as_live_until`, half way) draws from events alone and is
  dimmed; `machine` draws the nothing-to-split screen with the commands.
- Keys: down then enter opens `request` narrowed to the request; `d` moves to
  the second decomposition; a focus on a tid narrows to that bar, and one on a
  tid not drawn keeps every bar and says so.

- `tests/view_timebar.rs` runs the triage itself (`latency::read`, then
  `conclude`) on `sqlite-latency`'s results and asserts the timebar names
  the same state: `blocked` on both.

Reasoned, not shown:

- That `omn latency` *live* names `blocked` on this program. The parity test
  runs the triage on the recording, which holds `sched.offcpu`'s stacks and
  not its events, so the triage too leaves `blocked` unsplit; live it holds
  the events and would probably name `blocked: uninterruptible`. Once core
  re-records the fixture with the events (below), the same test holds the
  split.
- The bracket's lower end assumes the sched sources saw every thread of the
  process: a thread that never blocked and never waited for a cpu would be
  missing from the subtraction and the lower end too high. On the fixtures
  every thread of the target is seen by `sched.switches` too, but the view
  does not cross-check it.
- That the view's cost stays small live: events are grouped by thread once a
  draw, not per thread, but a draw still re-reads every result (the view
  keeps no cache between frames). Not measured on a 200 000-event live run.
- The colours: I read the `.ansi` escapes, not a terminal. The text snapshot
  reads on its own (a distinct cell per state, labels inside the bars).

## What I could not do, and why

- **Split `blocked` on the `sqlite-latency` fixture.** The recording holds
  `sched.offcpu`'s stacks, not its events (`omn latency`'s session asks
  `stack`); the split needs the events. Filed below for core.
- **Give a multi-threaded process's cpu to its threads exactly.** The
  catalog's per-thread backends of `cpu.oncpu_time` (`bpf:raw_tp`, `bpf:tp`)
  have no collector in this build (`src/collect/cpu/mod.rs` registers
  `fs:proc` only); the bracket is what the run supports.
- **Show the live view in a terminal.** The agent shell has none (`omn top`
  is refused at once there, by design); the live state is tested through
  `as_live_until`, whose fixture recorded `cpu.oncpu_time` without `raw`, so
  live on-cpu shows as unread in the snapshot. A real live run carries the
  poll's events. Listed below as a run for the user.
- **Follow a wait into the task that ran in it**, as `omn latency` does. The
  view draws both tasks side by side instead; following needs the triage's
  `Tasks::follow`, which the view crate cannot call.

## Findings beside the view

- `sched.wakeup`'s events are keyed on the wakee and carry the wakee's `pid`
  but the **waker's** `comm` (a wakeup of `sqlite3` reads `comm: omn`). A
  reader naming threads from every sched source names the launched program
  `omn`; the view reads names from `sched.offcpu`, `sched.runqlat`,
  `sched.switches`, `sched.futex` and `sched.mutex` only, and the latest name
  a thread had (a launched program is `omn` until it executes).
- `cpu.oncpu_time` through `fs:proc` keys its series `task:<pid>` for a
  per-process total: the key reads as a thread's. The view treats it as the
  process's; a reader that takes the key at its word attributes every
  thread's cpu to the main one (on `http-server`, 2.70 s of on-cpu to a main
  thread that was asleep 2.76 s of a 2.84 s window).

## Privileged runs wanted

None privileged. One run for the user, in a terminal, to judge the view live
(the agent shell has no terminal):

```console
cd /home/moha/repos/omniscient && make
for i in $(seq 40); do echo "insert into t values($i);"; done > /tmp/commits.sql
sqlite3 /tmp/commits.db 'create table if not exists t(x);'
target/debug/omn latency --objective latency --dump-session -- \
    sqlite3 -init /tmp/commits.sql /tmp/commits.db .quit > /tmp/s.yml
target/debug/omn top -f /tmp/s.yml            # then 5: the reading live, the decomposition at the end
target/debug/omn top -- python3 -m http.server --bind 127.0.0.1 8000   # 5, while a client lists a directory
```

## Decision lines for DECISIONS.md

| Decision | Why |
|---|---|
| The timebar's first suspect restates `omn latency`'s rule (largest state, ahead of the next at its most and of what no row read) in the view crate, held to the triage itself by `tests/view_timebar.rs`, which runs `latency::read` and `conclude` on every fixture carrying its decomposition | the view crate cannot depend on `omn`; a suspect by another rule would be a second answer to one question |
| A run without a decomposition by task is drawn as the view's own per-thread reading, said on screen to be the view's, with the commands that give `omn latency`'s | six of seven fixtures and every live run carry no decomposition; an empty view there would hide what the traced sources do say |
| A per-process cpu total is drawn as a bracket per thread -- least: the total less a tick less what the process's other threads could have run; most: what the thread's own states leave of the window -- never apportioned | a share of a total that no source attributes is a number that was not measured |
| A `blocked` row from a recording without `sched.offcpu`'s events is drawn whole, named by the run's heaviest blocking stack, and said not split | the split by task state needs the events; guessing it from a stack is a disk-or-sleep call the data does not make |

## Needs from another stream

- **core** (`tests/view_fixtures/record.sh`, `docs/view/README.md`):
  1. Record `sqlite-latency` with `sched.offcpu`'s events beside its stacks
     (append `raw` to the `blocked` request of the dumped session before `omn
     top -f`), so the timebar splits `blocked` as `omn latency` does -- and
     names `blocked: uninterruptible` rather than `blocked`. Re-recording
     changes the timebar's `sqlite-latency` snapshot: re-bless it with
     `OMN_VIEW_BLESS=1 cargo test --test view_snapshots timebar`.
  2. Done in the review round instead of asked: the timebar's `NEEDS` now
     carry `cpu.oncpu_time` (as the system and ledger views ask it, so the
     requests merge) and `sched.offcpu`'s `raw` beside its stacks (as lanes
     asks it), with the two README rows `tests/view_model.rs` holds them to.
     Core: review those rows, and take `tests/view_timebar.rs` (the parity
     with `omn latency`) as yours.
- **the sched family's owner** (no stream this wave): `sched.wakeup`'s `comm`
  is the waker's on an event keyed on the wakee (above). Either say it in the
  catalog's caveats or name the attribute `waker_comm`.
- **the cpu family's owner** (no stream this wave): a `bpf:raw_tp` collector
  for `cpu.oncpu_time`, which the catalog declares and the build lacks, would
  turn the timebar's on-cpu bracket into an exact per-thread figure.

## The suite

While iterating: `cargo test -p omn-view --lib timebar` (13 tests) and
`cargo test --test view_snapshots timebar`; `cargo clippy -p omn-view
--all-targets -- -D warnings` clean.

`make check` before the last commit: red, in `tests/cases.rs`
(`every_case_names_its_known_cause_both_ways`: a case's on-cpu 28 ms off its
reference, beyond the 26.8 ms the resolutions allow) -- a timing tolerance on
a machine shared by every stream, nothing of the view's.

Then one `cargo test --workspace --no-fail-fast`: **2359 passed, 20 failed,
11 ignored**, 10 targets red, none of them the view's: `cases`,
`family_sched`, `join_engine`, `runner`, `scenario_mem`, `unwind`, `omn-mcp`'s
`protocol` and `walkthrough`, and both doc-test targets. The doc tests failed
with `Disk quota exceeded` (rustdoc could not write its argument file), and
the quota is the likeliest cause of most of the other launches failing too:
the home filesystem was full while fourteen streams built and tested at once.
Every `omn-view` test and every `view_*` test passed (the timebar's 13, its
snapshots on every fixture). The red targets want one rerun once the disk has
room, by the coordinator, alone: they are not the timebar's to fix.

## Review

The adversarial review's seven findings, each fixed:

1. **Folding by the lower end (defect).** Fixed: `fold` keeps a bar when
   the most its states can be (`Bar::most_read`, a bracket at its upper end)
   reaches 1 % of the window, and the bars are ordered by the same figure. A
   busy worker beside a short-lived sibling (lower end zero, upper end the
   window) is drawn; test
   `a_thread_is_folded_only_when_its_most_is_a_sliver`. No fixture changed:
   `ldconfig` on `http-server` is still folded, its on cpu not being read at
   all (it is a process `cpu.oncpu_time` did not measure).
2. **No parity with `omn latency` (gap).** Fixed:
   `tests/view_timebar.rs` builds the triage's `Run` from each fixture's
   `results.json`, runs `latency::read` and `conclude`, and asserts the
   timebar's verdict on the same task's bar names the same state (or none).
   It holds on `sqlite-latency` (`blocked`), and fails if no fixture is
   held. The deliberate difference (`next` at its most) is said in the
   module doc and `docs/view/timebar.md`: it only differs on a bracketed
   row, and a decomposition has none.
3. **A residue that cannot fail (gap).** Fixed: when on cpu is derived from
   the window, the bar has no residue and the table says `not measured: on
   cpu is derived from the other rows`; test
   `a_residue_that_cannot_fail_is_not_printed_as_one`. A per-thread on-cpu
   source (a `bpf` backend) would still give a measured residue.
4. **`NEEDS` incomplete (gap).** Fixed here rather than filed: see "Needs
   from another stream", item 2. The nothing-to-split screen now lists them.
5. **"Adds up to the row" (nit).** The handoff's claim is corrected, the
   test renamed `blocks_split_by_state_partition_the_events`, and a split
   whose events disagree with the row now moves the residue by the
   difference, with a note.
6. **A weak live test (nit).** It now asserts that the screen says it is the
   view's reading and draws the first bar's own verdict with its label.
7. **Claims without checking (nit).** A thread with no block is said to have
   never blocked only when `sched.offcpu` lost nothing, followed every
   process (no `not followed` loss reason) and reported no task still off cpu
   at the end; otherwise its zero is a lower bound, drawn `≥`, with the
   source's words. A decomposition not drawn (not in `ns`, unreadable, no
   table) is said on every sheet and on the nothing-to-split screen; test
   `a_decomposition_not_drawn_says_why`.

