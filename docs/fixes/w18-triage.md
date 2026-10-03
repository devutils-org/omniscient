# The eighteenth wave — stream triage

Branch `feat/w18-triage`, from `main` at `9cd5409`. The files:
`src/triage/{latency,engine}.rs`, `src/triage/workload.c`, `tests/triage.rs`;
outside them, one line of `src/main.rs` and the four
`examples/triage-latency-*.yml` (section 6, both said there).

| Item | State | Commit |
|---|---|---|
| A wait for a child followed into the child that ran during it | landed | `5ba95b6` |
| The busiest other thread named beside a block it ran in | landed | `5ba95b6` |
| The verdict says which task its state is from (`verdict.task`, the followed tables) | landed | `5ba95b6` |
| `sched.mutex` asked only when a futex sleep needs splitting (a second pass) | landed | `5ba95b6` |
| A source's cost said when it is over 20 % of the target's cpu | landed | `5ba95b6` |
| `Limit::Sleep`'s sentence names a polled lock and a wait for a child | landed | `5ba95b6` |
| `--symbols` on every latency request that reads stacks | landed in `latency.rs`; the flag is names', the wiring launcher's (section 7) | `5ba95b6` |
| Held by a workload whose work is in a child, and one whose work is in a thread | landed | `5ba95b6` |

## 1. Which task (`src/triage/latency.rs`)

As decided before the wave: no `--thread` option, both candidates.

- **The reading follows its largest row.** After a task's table is read, if
  its largest row is a `blocked` state named `sleep` or `futex` (not a lock,
  not a disk), the run is asked who ran in it, in this order:
  1. **a wait for a child**: the heaviest blocking stack of that state goes
     through a function named `wait4`, `waitpid`, `waitid`, `wait3` or `wait`
     (`is_wait_for_child`, the C library's own copies included), and a task
     of another process is in the run: the child that ran longest -- the
     task, among processes not already on the way, whose rows account for
     the most, a process's main thread standing for it -- is followed **in
     the wait's place**. Its whole is the wait, not its table's whole, which
     is the program's life (w17-build's "a child's `whole` is the launched
     program's life").
  2. **another thread of the same process** whose rows account for at least
     a quarter of the block (`RAN_IN_IT`), the busiest by on-cpu plus
     runnable: followed **beside** the block. Its whole is what its own rows
     add up to, since no source carries a thread's life; its residue is not
     stated.
  3. **a child with no wait on the stack** (a pipe read the child closes)
     that accounts for a quarter of the block: followed in its place.

  A followed task is read the same way, up to `MAX_FOLLOWED` (8) deep, never
  into a task or a process already on the way: a driver waiting for a
  compiler it started is followed to the compiler, make to the job's driver
  to its compiler.
- **Which process a task is in** comes from the `pid` the sched sources
  carry beside the task. `cpu.profile` carries none, so a task only the
  profile saw is, by elimination, not one of the program's threads (which
  the sched sources watch) -- another process. Section 7 asks profile for the
  `pid`.
- **A child's zeros are not zeros.** The sched sources do not follow the
  target's children yet (stream sched's first bullet), and the decomposition
  writes a source's silence about a task as `0`: cc1's table in w17-build was
  `runnable 0.0, blocked 0.0` while it waited 2 s for a cpu. A followed task
  of another process whose row has no event is now **unread**, with the
  reason, so until stream sched lands, a followed compiler is answered
  `undecided` with its unread rows named, where the seventeenth wave answered
  the driver's `wait4` with confidence. Once sched follows the descendants,
  the rows are read and the verdict decides.
- **The other way round for the program's own threads**: a `blocked` or
  `runnable` row with no event, from a source that watches every switch of
  the program's threads and lost no event (a late drain loses none), is now a
  zero, said in a note. A thread that spun 300 ms and never left its cpu read
  as `blocked: not read`, which made the cpu-bound verdict undecided by the
  whole wall clock (section 2, the lossy-profile test).
- **The verdict** reads the last reading of the chain (`Reading::chain`), with
  `verdict.task` naming its task, and its sentence says how it got there:
  *"for latency, in the run, task:152191 waited 302.71ms (88.4 % of its wall
  clock) for a child, in `wait4`, and task:152318, the child that ran during
  the wait, is decomposed in its place: it spent most of its time on a cpu
  ... on-cpu 257.77ms (85.2 % of the wait it ran in); hottest function
  `spin_cpu_ms`"*. The thread's: *"task:167912 was `blocked: sleeping`
  309.48ms (93.6 % of its wall clock) while other threads of the process ran,
  and beside it task:168009, the busiest of them, is decomposed: it spent most
  of its time on a cpu ..."*. The main thread's blocking stack is not quoted
  in the sentence (http-server's `never_says: [select.cpython]`); it stays in
  the main thread's table. The text prints each followed table below the one
  it was followed from, indented, *"followed from `blocked: sleeping`
  (302.71ms), in its place:"* / *"beside it:"*.
- **The tail** subtracts the followed tasks' states when both runs were
  followed the same way, and the main threads' otherwise.
- **A verdict's room when every row was read** is the rows' own resolution,
  no longer the residue's resolution, which carries the whole's bracket: on a
  loaded machine the program's exit is seen up to 60 ms late, and that moves
  the residue, not a row. When a row is unread the room is as before.

JSON: `Reading` gains `followed` (`relation` `child`|`thread`, `during`,
`amount`, `wait`, `reading`) and `costly`; `Verdict` gains `task`; `Answer`
gains `second_pass`. Nothing was renamed or removed.

## 2. `sched.mutex` and what a source costs

- **Two passes.** `Ask::session()` -- what `--dump-session` prints and what
  the agent's way runs -- no longer asks `sched.mutex`; `Ask::split_session()`
  does. `answer` prunes the full session (so `dropped` still names `mutex`
  when it cannot be served), runs the first pass without it, and runs the
  whole question again with it only when the reading the verdict reads, or
  one on its way, has a futex sleep as its largest row -- or any sleep, when
  `sched.futex` was not there to say -- (`needs_split`). The second pass is
  the answer, and `second_pass` says why it ran and what the first said. For
  `--pid`, the second pass is a second window as long as the first, and says
  so. No backend of `sched.mutex` is under a ceiling (all three are
  `extreme`), so "through a backend under its own ceiling" has nothing to
  choose.
- **The lock workload** now runs twice: the first pass names the futex sleep
  (and, followed, the holder thread spinning beside it), the second names the
  lock, 158.45 ms in `pthread_mutex_lock` against the program's own 158.58 ms.
- **A source's cost** (`engine::Run::costs`, from each collection's
  `Overhead::percent`): over `COST_CRITERION_PERCENT` (20 %, where the
  `extreme` band starts) it is said under the verdict, *"cost: [mutex]
  sched.mutex through `preload` cost 127.0 % of the target's cpu (...), over
  the 20 % past which a source dilates what it measures; declared
  `extreme`"*. The denominator is the runner's (stream launcher's first
  bullet); a driver whose children did the work reads thousands of percent
  until it weighs the descendants.

## 3. `Limit::Sleep` and `--symbols`

- *"asleep on something that is not a lock seen here: a timer, a pipe, a
  socket, a lock polled with a sleep between tries, or a wait for a child
  process"*.
- `Ask` gains `symbols: Option<SymbolSource>`, set on the two requests that
  read stacks (`on-cpu` through `cpu.profile`, `blocked` through
  `sched.offcpu`) and on no other.

## 4. Shown, and reasoned

**Shown** on the development machine (24 cpus, 7.2.7, paranoid -1, `omn` with
the capabilities from `make`, nine other streams running):

- `cargo test --lib triage::latency`: 19 passed, eight of them new (a wait
  for a child answered by the child; a child whose rows were not watched is
  undecided, naming them; a thread named beside the block; a tail of followed
  runs; only a futex sleep asks for a second pass; the first pass asks no
  mutex and both stack requests carry `--symbols`; the wait functions by
  name; the sleep sentence).
- `cargo test --test triage`: 27 passed, with the capabilities (checked
  with `getcap` before the run; a relink drops them and the tests then skip
  -- my first run did, and is not counted). New:
  - `a_program_whose_work_is_in_its_child_is_answered_by_that_child`:
    `workload child 300 2`, a driver that waits in `waitpid` for a child that
    waits for a grandchild that spins. The verdict decomposes the grandchild
    (`verdict.task` is its tid, from its own `omn-truth` line), never the
    driver's sleep, and its on-cpu row is the grandchild's
    `/proc/thread-self/schedstat` within three periods and 2 %, less what
    the profile says it missed by finding the task a tick late (257.77 ms
    against 300.04 ms, 43.41 ms found late, on the run above). Decided `cpu`
    on this machine, because the two unread rows were small beside the
    on-cpu one; the test accepts undecided only with the unread rows'
    reason.
  - `a_program_whose_work_is_in_another_thread_names_that_thread_beside_its_wait`:
    `workload thread 300`, the main thread in `read(2)` on a pipe while a
    second thread spins. Followed as `thread`, during `blocked: sleeping`,
    the verdict's task is the worker, its limit is the larger of the
    worker's kernel figures (on a cpu or waiting for one), its on-cpu row
    agrees as above (265.80 ms against 300.04 ms, the same late discovery),
    and the main thread's sleep is still in the answer.
  - The cpu-bound test asserts no second pass and nothing followed; the lock
    test asserts the second pass and why; the dump test asserts the dumped
    latency session asks no `sched.mutex`.
- Run by hand on `gcc -O2 -c workload.c`: refused for the capabilities a
  test relink had dropped; not rerun (section 5).
- **Failures seen, and read**: `a_verdict_read_off_a_lossy_profile_...`
  failed three times in a row before the room change in section 1 -- first
  on the blocked row unread (the spinner never blocked), then on the
  lifetime's bracket. The **main checkout's binary at `9cd5409` fails it the
  same way on this machine** (undecided three times of three: *"`on-cpu` is
  133.67ms, and 238.98ms of the wall clock is in no row that was read"*), so
  it is the load, not this branch; with both changes it passes.
  `a_contended_mutex_is_what_the_workload_queues_for` (saturate) and
  `the_same_program_is_cpu_bound_..._for_the_tail` each failed once and
  passed when rerun alone.

**Reasoned, not run**:

- **The three cases** (`make-sqlite`, `http-server-stdlib`, `gcc-shell`) were
  not run: `tests/cases.rs` is phase B's, and its reader takes
  `runs[0].states` and the `named` lines of the verdict's state, which are
  now the main thread's, not the followed task's (section 7). What they
  should show once read the new way:
  - `http-server-stdlib`: the main thread's `select` is `sleep`; the serving
    thread is in the same process and accounts for 4.34 s of a 4.39 s block
    (the listing) and 1.29 s of a 2.29 s one (the slow client, over the
    quarter), so it is followed beside it: `cpu` for the listing, `sleep`
    (its send) for the slow client, and the sentence does not quote
    `select.cpython`.
  - `gcc-shell` and `make-sqlite`: `wait4` on the stack, followed to the
    child that accounts for the most. Until stream sched lands, cc1 is read
    from the profile alone and the verdict is undecided with
    `runnable`/`blocked` unread; with it, the contended compile's cc1 reads
    `runnable` (2.1 s against 1.05 s on a cpu) and the `-O2` compile `on-cpu`.
    make goes make -> the shell job's driver -> its cc1, the job that lasts
    99.9 % of the build being the one that accounts for the most.
- **Without `pid` on profile samples**, a child the sched sources saw is
  placed in its process, and one only the profile saw is taken as another
  process by elimination; a thread of the program the sched sources never
  saw at all (never switched, never queued) would be taken for a child, and
  followed only if the stack names a wait or it accounts for a quarter of
  the block.
- **Which child, without parentage**: no source carries a parent pid, so
  "the child" is the process not yet on the way that accounts for the most,
  which for a driver is its compiler and for make its longest job; a sibling
  that ran longer than the waited-for child would be taken for it.

## 5. What I could not do, and why

- **The cases, both ways**: phase B (stream cases), and the reader needs the
  change in section 7 first.
- **`omn latency` on a real gcc** by hand: the test run before it relinked
  the binary and dropped the capabilities; I rebuilt with `make` for the
  tests and did not go back. The workload's `child` mode is the same shape.
- **`--symbols` end to end**: the flag is stream names' (`src/cli.rs`), and
  the line passing it is in `src/main.rs` (stream launcher's).
- **gcc-shell's return from `cases/held/`**: the cases stream's. It
  **should return** in phase B once stream sched's descendants are merged:
  then the person's way reads cc1's own rows and names its cause. Without
  sched, the person's way answers undecided rather than wrong, which still
  fails `the-first-suspect-is-the-known-cause`; and the agent's way reads the
  heaviest table of `run_session` (w17-build, "not filed as a check"), which
  nothing here changes. So: return it if both hold after phase A's merge,
  keep it held naming the one that does not.

## 6. Two files outside the row, and why

- `src/main.rs:2202` (stream launcher's): `symbols: None` in the `Ask` the
  command builds, because a field added to a struct built by literal does
  not compile otherwise. One added line in a literal launcher's bullets do
  not touch.
- `examples/triage-latency-{cpu,io,lock,tail}.yml` (stream engine's): the
  `sched.mutex` request removed, and the lock example's comment saying the
  second pass names the lock. `tests/triage.rs`'s
  `every_triage_session_is_what_its_command_asks_for` holds each example
  equal to what its command dumps, so the dump and the examples change in
  the same commit or `main` is red between two merges. Pure deletions in
  files engine's bullets do not name.

## 7. Needs from another stream

- **names** (`src/cli.rs`, `docs/mcp.md`, `man/omn.1`):
  - `--symbols` on `LatencyArgs`; then the line in section 6 becomes
    `symbols: args.symbols` (launcher's file: one of the two of you, or the
    coordinator at the merge).
  - `docs/mcp.md` and `man/omn.1`: the latency session asks four sources,
    `sched.mutex` only in a second pass the command runs itself; the answer's
    `followed` readings and `verdict.task`; the new `Limit::Sleep` sentence;
    `cost:` lines over 20 %. For the agent's way, a sentence on reading the
    tables the way the command now does: when the main thread's largest row
    is a `wait4` (or a block another task ran in), read the child's or the
    busiest thread's table, not the heaviest.
  - The hottest function by perf-map frame, the flat interpreter profile and
    the block's stack to the program's frame (python 1 to 3) are
    `latency.rs` lines (`hottest`, `blocking_stack`): I did not touch them,
    to leave them to you whole; if you would rather I land them, say which
    and I will.
- **profile** (`src/collect/cpu/profile.rs`): carry `pid` beside the task on
  every sample, as the sched sources do, so a child the profile alone saw is
  placed in its process rather than by elimination. And the late discovery:
  a child that ran 300 ms lost 43 ms (14 %) to *"found on a tick after they
  began"*, and a thread 34 ms of 300; the followed task's on-cpu row is short
  by it.
- **sched**: following the descendants (your first bullet) is what turns a
  followed child from `undecided` into an answer: its rows with no event are
  read as unread until a source reports them.
- **launcher**: `symbols: args.symbols` (above); and the budget's
  denominator, which is also what the `cost:` lines divide by.
- **cases** (`tests/cases.rs`, phase B): `read_cli` takes `runs[0].states`
  and the `named` lines of the state named `verdict.state`; with a followed
  task those are the main thread's. Read the last reading of the chain
  (`runs[i].followed.reading`, repeatedly, until no `followed`) for the rows
  and the `named` lines, and `verdict.task` for whose they are. The
  `agrees:` checks of `http-server-stdlib` (the serving thread's figures) and
  `gcc-shell` (cc1's) then compare like with like.
- **coordinator**: `GUIDE.md`'s latency session, if it lists five sources or
  quotes the old sleep sentence.

## 8. Privileged runs wanted

None for this stream's own claims: everything above ran with the capabilities
`make` grants. One run belongs with phase B rather than here: the three cases
at `perf_event_paranoid` 2, which the coordinator already lists.

## 9. Decision lines for `DECISIONS.md`

- **Which task the triage decomposes** (moves from "Open" to settled): the
  main thread, and then the task that ran in its largest row when that row is
  a sleep or a futex wait: a child, in the wait's place, when the blocking
  stack names a wait for a child or the child accounts for a quarter of the
  block; another thread of the process, beside the block, when it accounts
  for a quarter of it. Followed the same way down to eight tasks. No
  `--thread` option. *Why*: it needs nothing from the person asking, and the
  three cases it turns are all one shape -- the program's work in a task its
  main thread waits on. `tests/triage.rs`,
  `a_program_whose_work_is_in_its_child_is_answered_by_that_child`,
  `..._in_another_thread_names_that_thread_beside_its_wait`.
- **A followed child's silent row is unread; the program's own thread's is a
  zero**: a source that does not follow children says nothing about them,
  and a source that watches every switch of the program's threads and lost
  no event says "never" by saying nothing.
- **`sched.mutex` is asked in a second pass**, only when a futex sleep (or,
  without `sched.futex`, any sleep) is the largest row on the verdict's way;
  never in the dumped session. *Why*: every backend is `extreme`, and on a
  program that locks often it doubled the time it was asked to explain
  (python 5, build's mutex shim); a second run of the program is the price,
  paid only where the split can change the answer.
- **A source's measured cost over 20 % of the target's cpu is said beside
  the verdict**, the line where the catalog's `extreme` band starts.
- **When every row was read, a verdict's room is the rows' resolution**, not
  the whole's bracket.

## 10. The suite

One `make check` on `5ba95b6`'s tree: red at `tests/budget.rs`
(`the_extra_cpu_a_faster_poll_costs_is_the_extra_cpu_the_shell_observes`,
launcher's file). Then one `cargo test --workspace --no-fail-fast`, with the
capabilities (`getcap` before the run): **2 268 passed, 16 failed, 12
ignored**, every failure outside this stream's files and none in
`tests/triage.rs`: `cases` (a cpython on-cpu row 0.9 ms beyond its
resolution, and sqlite3's leaf naming a kernel frame -- not the followed
reading, which `read_cli` does not read yet), `scenario_cpu`, `scenario_gpu`,
`scenario_mem`, `family_sched`, `recorder`, `runner`, `unwind` (three),
`vertical` (`io.latency` lost 30 % of its events). Most are in the flaky
tally; the three `unwind` ones are not, and rerun alone gave a different
three, and `dwarf_recovers_the_whole_tree_...` alone passed: the load of ten
streams, in profile's file. Clippy and fmt clean.
