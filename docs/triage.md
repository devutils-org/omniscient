# Triage — the layer above the engine

`ROADMAP.md` phase 10, everything but the escalation. The tenth wave's stream
T. This is the deliverable, not a log about one: what the layer is, what it
answers and against what ground truth, what it does not do and why, what it
asked of the files it does not own, and the rows the coordinator pastes.

```console
omn latency  --objective latency|tail        [--runs N] [--pid PID -d DUR | -- PROGRAM]
omn saturate --objective throughput|tail|energy          [--pid PID -d DUR | -- PROGRAM]
omn diff BEFORE AFTER [--format text|json|folded]
```

---

## What it is

**It sits above the engine and calls it like any other user.** Each method
writes a `Session`, runs it through the calls `omn -f` makes — validate,
resolve, `collect::run_all_within`, `emit::aggregate`, `join::apply`,
`join::decompose` (`src/triage/engine.rs`) — and reads the answer off the
results. It edits nothing in the collector layer, the join engine or the emit
layer. `--dump-session` prints the session a question is asked through; `omn
validate` accepts it and `omn -f` runs it, which a test checks for every one
of them, and `examples/triage-*.yml` are those sessions, held to what their
command dumps by another test.

What the layer computes itself is arithmetic on numbers the engine reported —
a share, a Little's-law product, a difference of two profiles — and every
row's kind, evidence level, resolution and `Shortfall` is carried through to
the answer rather than summarised away. A value from a lossy run prints behind
`>=`, in the verdict as in the table.

| File | What |
|---|---|
| `src/triage/objective.rs` | the objective, required, and which method answers which |
| `src/triage/engine.rs` | the engine, called as a user; optional inputs dropped by name |
| `src/triage/latency.rs` | `omn latency`: TSA as a question |
| `src/triage/saturate.rs` | `omn saturate`: USE with Little's law |
| `src/triage/diff.rs` | `omn diff`: differential profiles |
| `src/triage/workload.c` | the ground-truth fixture every scenario launches |
| `tests/triage.rs` | 17 scenarios: ground truth, refusals, the loss path, the capability sweep |

---

## The objective comes first

A triage request that does not name one is refused with the list — exit 2, the
validator's code, nothing measured:

```
$ omn latency -- ./prog
error: `omn latency` needs an objective, and does not pick one: median latency, the tail,
throughput and energy do not name the same culprit, and an answer for the wrong one is a
confident wrong suspect. Say which with --objective:
  latency     how long a typical run takes: the median run, decomposed
  tail        how long the slowest runs take: what the slowest run spent that the median did not
```

Four objectives, and each method answers the ones it can. The others are
refused with the reason and the command that does answer them:

| | `omn latency` | `omn saturate` |
|---|---|---|
| `latency` | the median run, decomposed | refused: a resource walk adds every task's time, and a latency is one task's; names `omn latency` |
| `tail` | the slowest of `--runs` against the median (3 runs or more; refused on `--pid`, which runs once) | the resource whose single waits have the longest p99 |
| `throughput` | refused: a wall clock is not a throughput; names `omn saturate` | the longest queue behind something another task holds |
| `energy` | refused: a blocked nanosecond costs the cpu nothing; names `omn saturate` | the package's energy, credited by cpu time |

`cost` is not an objective: nothing in this tool measures a price, and an
objective the tool cannot measure would be answered by one it can.

---

## `omn latency` — why is this slow

TSA, which `examples/join-decompose-compute.yml` already produced as a table,
asked as a question. The session has five requests — `cpu.profile` (on-cpu),
`sched.runqlat` (runnable), `sched.offcpu` (blocked, with stacks),
`sched.futex` and `sched.mutex` (optional: what a sleep was waiting on) — and
one `decompose:` over the program's lifetime, or the window for `--pid`.

Three things the table alone does not say, all read off the same run:

- **Which task.** A program's latency is its main thread's life — the thread
  whose id is the process id, found on any event carrying both. The other
  threads are listed on one line each, not averaged in.
- **What the blocked time was blocked on.** The row is split by the task state
  `sched.offcpu` recorded at every switch, with the row's own arithmetic
  (`total_ns` less `queued_ns`, per state), so the states add up to the row:
  `uninterruptible` is `disk`; `sleeping` is named by the lock source that
  covers more than half of it — `sched.mutex` is `lock`, `sched.futex`
  `futex` — or else `sleep`, with the heaviest stack the blocks began from.
  On-cpu time is named by its hottest leaf function.
- **Whether the largest state is decisive.** It is named only if it exceeds the
  next state *and* everything no row read — a row the run did not supply, the
  whole less what was read, the rows' resolution. Otherwise the answer is
  undecided and says why.

For `tail`, the slowest run's states are subtracted from the median's; the
state whose time grew most is named when it exceeds the next difference and
both runs' residues and resolutions. The slowest of N is their maximum, and
below a hundred runs the sentence says it is not a percentile.

**Measured here** (each a real run of the fixture; the numbers are this
machine's, the verdicts are the fixture's by construction):

```
cpu 200     for latency, task:102746 of the run spent most of its time on a cpu, doing its
            own work: cpu-bound: on-cpu 202.61ms (99.8 % of its wall clock); hottest
            function `spin_cpu_ms`, 184 of 202 samples (91 %)
            kernel's own figure for the thread: 201.31 ms on a cpu

io 200      ... blocked uninterruptibly, which is a device and usually a disk: I/O-bound:
            blocked: uninterruptible 180.79ms (84.0 %); 100 % of it from one stack, leaf
            first: fdatasync <- main <- ...
            the program's own clock inside fdatasync: 211.81 ms (which includes the cpu
            btrfs spends there: 28.9 ms on a cpu in all)

lock 100    ... asleep in a pthread mutex another thread held: lock-contended: blocked:
2000        sleeping 107.19ms (97.2 %); 99.53ms waiting in pthread_mutex_lock over 101
            acquisition(s), most of it on the mutex at 0x56531c528140
            the program's own clock around pthread_mutex_lock: 99.55 ms

tail, 5     for the tail, the slowest run took 200.16ms more than the median, and 200.05ms
runs        of it is more time asleep on something that is not a lock: blocked: sleeping;
            100 % of it from one stack: clock_nanosleep <- usleep <- mode_tail <- main
            the same five runs, --objective latency: on-cpu 102.31ms (99.5 %), cpu-bound
```

The last pair is the workload whose answer changes with the objective, and the
reason the objective is required: the median run is cpu-bound and the slow run
is slow because it slept.

---

## `omn saturate` — what does this queue for

USE per resource, from the workload's side, with **L = λW** as the arithmetic
and every term printed. A traced source hands up every wait with its length,
so the sum over the window is exact: `L = ΣW / T`, which is also the arrival
rate times the mean wait, and the text prints both.

| resource | utilization | saturation | errors |
|---|---|---|---|
| cpu | cpus busy with it: the reap's rusage / T (polled `cpu.oncpu_time` for `--pid`) | tasks waiting for a cpu: Σ `sched.runqlat` / T | not per process: `cpu.throttle` is a cgroup's |
| disk | requests in flight at the device: Σ `io.latency` / T | tasks blocked on a device: Σ uninterruptible / T; the device's own queue depth (`io.inflight`, every process's) beside it | requests completed with an error status |
| lock | a mutex held: Σ `hold_ns` / T, at most 1 | threads waiting for it: Σ `wait_ns` / T (`sched.futex` when no pthread mutex was seen) | — |
| memory | — | the cgroup's memory stall share (PSI), marked TEMPORAL: the cgroup holds more than the workload | — |

- **throughput** names the longest queue of the workload's own work *behind
  something another task holds* — a cpu or a lock — when it reaches a tenth of
  a waiter on average. A task blocked on its own disk request is not queued
  behind anyone. Below the line nothing is saturated, and the answer says the
  workload is limited by its own concurrency, with the cpus and in-flight
  requests it actually keeps busy.
- **tail** names the resource whose single waits have the longest 99th
  percentile, read off each source's log2 histogram: known to its bucket, and
  undecided when two fall in the same one. The histogram's `Shortfall` is
  carried — finding 37 exists for this reader — and a lossy run's percentile is
  marked a lower bound.
- **energy** reads `power.energy` per domain and credits the workload with the
  package's energy times its share of the machine's busy cpu time
  (`cpu.util`), marked TEMPORAL because the meter is the package's. The disk's
  and the network's energy are named as unmetered, not as free.

**Measured here:**

```
queue 4 250   for throughput, the workload queues for the cpu: 2.984 of its tasks runnable
              and waiting for a cpu, on average
              little  wakeups waiting for a cpu: L = λW = 392.7/s x 7.60ms = 2.984
              (four threads pinned to one cpu: three wait whenever one runs)

lock 150      for throughput, the workload queues for the lock: 0.896 threads waiting for it
2000          (the mutex held 95 % of the time)

io 200        for throughput, no queue of the workload's own work reaches 0.1 waiters on
              average: nothing it uses is saturated, so its throughput is limited by its own
              concurrency ... Its tasks are blocked on a device 86 % of the time, each on
              its own request: that is a latency to shorten or to overlap, not a queue to drain.

cpu 200       for energy, what this machine meters of the workload is its cpu time: it held
              13.8 % of the machine's busy cpu time, so about 3.28 J of the package's energy
              is its -- credited by cpu time, which is an estimate (TEMPORAL)
```

The `io` workload is the second answer that changes with the objective:
`omn latency` names the disk, `omn saturate --objective throughput` names
nothing, `--objective energy` names the cpu.

---

## `omn diff` — two profiles and a subtraction

Inputs are a run's `-o` directory (`results.json` or `results.folded`, with
`resolved.json` for the rate), a `results.json`, or a folded file. A run's
results go through the same `folded` encoder a run writes with, so both inputs
are one format by construction; the differential file is the three-column
`stack before after` that `flamegraph.pl`-style tools draw.

- **Absolute, not shares**: when one function doubles every other's share
  falls, and comparing shares would name all of them. Same rate: samples. Two
  rates: time. Unknown rate (a folded file): samples, and it says so.
- **Own (leaf) cost** decides; the inclusive table shows what carried it.
- **Against Poisson noise**: a function is the difference beyond three
  standard deviations, `|b − a| / sqrt(a + b)`.
- **With both runs' losses**: a `Shortfall` on either side is named; a folded
  file carries none and the answer says it could not check.

**Measured here**, the fixture's four functions of equal cost with `beta` run
once, then twice:

```
the difference is `beta`: +104 samples of its own (x1.99, 5.9 standard deviations);
every other function moved within its noise
  beta     105  209  +104  1.99  5.9 *
  delta    104  105    +1  1.01  0.1
  alpha    105  105    +0  1.00  0.0
  gamma_   105  105    +0  1.00  0.0
```

and the same program run twice unchanged: no function beyond the noise.

---

## Ground truth

Every scenario launches `src/triage/workload.c`, which `tests/triage.rs` builds
into `target/omn-triage-fixture/workload`. Each mode spends its time the way
the test chose and prints the kernel's accounting of its main thread
(`/proc/thread-self/schedstat`) and its own clock around the part that matters,
prefixed `omn-truth`. The tool reads none of it.

| Scenario | Chosen | Checked against |
|---|---|---|
| `a_cpu_bound_program_is_named_cpu_bound_for_latency` | 300 ms of spinning | verdict `cpu`; on-cpu within 3 periods + 2 % of the kernel's figure; hottest function `spin_cpu_ms` |
| `a_program_blocked_on_its_disk_is_named_io_bound_for_latency` | 300 × (write, fdatasync) | verdict `disk`; uninterruptible time ≤ the program's own time inside fdatasync; the blocking stack ends in fdatasync |
| `a_program_waiting_for_a_mutex_is_named_lock_contended_for_latency` | a mutex held 2 ms at a time, taken 150 times | verdict `lock`; the shim's wait within 5 % + 1 ms of the program's own clock around `pthread_mutex_lock` |
| `the_same_program_is_cpu_bound_for_latency_and_sleep_bound_for_the_tail` | 100 ms spin, 200 ms sleep in run 5 of 5 | tail: slowest run is the one the program says slept, verdict `sleep`, excess 199–210 ms; latency: `cpu` |
| `four_threads_on_one_cpu_queue_three_deep_by_littles_law` | 4 threads pinned to 1 cpu, against 1 | L(4) − L(1) within [2.5, 3.5]; verdict `cpu`; λ·W = L term by term |
| `a_contended_mutex_is_what_the_workload_queues_for` | the lock workload | throughput verdict `lock`; the mutex held more than half the time |
| `a_program_io_bound_for_latency_queues_for_nothing_for_throughput` | the io workload | throughput verdict none, "concurrency"; blocked on the device > half the time |
| `energy_is_credited_to_the_workload_by_its_cpu_time_and_says_what_is_not_metered` | 200 ms spin | verdict `cpu`; share in [0, 1]; TEMPORAL and the disk named |
| `the_function_whose_cost_doubled_is_the_only_difference` | `beta` × 1, × 2, × 1 again | difference `["beta"]`, ratio 1.6–2.4; unchanged pair: none; folded file carries both weights |
| `a_verdict_read_off_a_lossy_profile_says_its_amount_is_a_lower_bound` | a 100 kHz profile | the loss path: on-cpu `short`, verdict `lower_bound`, `>=` in the sentence |
| `an_input_this_machine_cannot_serve_is_dropped_by_name_or_fails_the_question` | a catalog without the lock sources, then without the run queue | the capability sweep: `dropped: [futex, mutex]` and still `cpu`; then a failure naming `runnable` |

And six that need no privileges: the objective refused with the list, an
objective refused with the command that answers it, the tails that cannot be
observed, the dumped session validating, every `examples/triage-*.yml` equal to
what its command dumps, and a profile that is not one refused by name.

**What is asserted is the separation, not the machine.** The queue is checked
as four threads against one on the same cpu, which a neighbour on that cpu
adds to both. The io workload's "nothing saturated" depends on this machine
not making it wait for a cpu, so the test reads the workload's own run-queue
share first and skips, with the number, above 5 %. No scenario skipped on the
development machine; `cargo test --test triage` is 17 tests in about 23 s.

---

## What it does not do, and why

- **The escalation** (phase 10 item 3) is the next wave's: it stands on hot
  attach, which stream H is building now.
- **A tail below a hundred runs is a maximum.** `omn latency --objective tail`
  compares the slowest of N with the median and says, under a hundred, that it
  is not a percentile. A tail within one long-running process — operations
  rather than runs — needs operation boundaries, which `net.request` has for
  a server and nothing has in general.
- **The disk's capacity is not read.** Requests in flight at the device are the
  device busy with this workload; whether the device could take more is not
  observable from here, so the disk is never named for throughput on its own
  occupancy.
- **Other threads' on-cpu time is not in the latency table**: `cpu.profile` is
  not inherited, and those rows read zero. The answer is about the main thread
  and says so; the request is filed.
- **The blocked row is split in this layer, not in the engine**, because a
  filtered part that matched nothing reads `not read`. Filed; the split uses the
  engine's own arithmetic on the engine's own events.
- **Energy is an estimate by cpu time**, marked TEMPORAL; RAPL meters a
  package, and on a machine shared with three building streams the package's
  energy is mostly theirs.
- **Memory is PSI only**, of the workload's cgroup, which holds more than the
  workload. Utilization of memory has no single number here worth naming.
- **No `--budget`.** A triage run pays for the sources its method needs and
  measures what they cost on every result; the budget ladder lives in
  `main.rs`'s `trace` path, and moving it into the library is not needed until
  the escalation, which is where a budget decides something.

---

## The manual

`man/omn.1` gained the three commands in `SYNOPSIS` and `COMMANDS`, a
`TRIAGE` section (the objective, the verdict rules, Little's law, the diff
arithmetic) and triage examples. `EXIT STATUS` carried the table from before
the exit codes changed — 2 for "this machine cannot serve it", which is 1 now —
and is fixed. `FILES` no longer counts the sessions.

**The eighth wave's request for `between` and `decompose:`** was already
answered in the manual when this wave started — the `Joins` section has
`between`, `Decompositions` has the block, and `EVIDENCE` defines `TEMPORAL`
by what produces it. What was missing is the part fields, and they are there
now: `less:`, `filter:`, `reading:`, `names:`, `by:` and `top:`.

**The ninth wave's request to say what `=~` does** is left, deliberately:
`tests/scenario_runtime.rs` asserts the manual is still silent and is not this
stream's, so landing the text alone would turn the suite red. It goes in at
the merge with that assertion; `docs/open-findings.md` says where both are.

---

## Filed in `docs/open-findings.md`

Under *Needs from another stream — the tenth wave, stream T (triage)*:

1. `src/join.rs`: a filtered part that matched nothing is a measured zero, not a
   missing row.
2. `src/collect/cpu/` or `src/perf.rs`: a profile that follows the threads a
   launched program starts.
3. The coordinator: a `triage-fixture` target in the `Makefile`, the shape of
   `vertical-server`, so `examples/triage-*.yml` run after `make` alone.
4. The coordinator: `=~` in the manual together with the assertion that waits
   for it.
5. The coordinator: the objective rules for `DECISIONS.md`.

No `mod` line is asked of anyone: `src/lib.rs` is this stream's and gained
`pub mod triage;`.

---

## Running it

```console
make                                  # capabilities; the eBPF sources need them
cargo test --test triage              # builds the fixture, runs the 17 scenarios
W=target/omn-triage-fixture/workload
omn latency  --objective latency -- $W lock 150 2000
omn latency  --objective tail --runs 5 -- $W tail /tmp/counter 5 100 200
omn saturate --objective throughput -- $W queue 4 250
omn latency  --objective latency --dump-session -- $W io 300 target | omn -f - --input-format yaml
```

---

## The suite on this branch

One `make check`, then one `cargo test --workspace --no-fail-fast`: every test
passes but three, none of them in this stream's files.

- `tests/cli.rs::the_session_indexes_hold_exactly_the_sessions_on_disk` — the
  five `examples/triage-*.yml` below, until their rows are pasted. Expected.
- `tests/aggregation.rs::two_emits_of_one_request_describe_the_same_events` —
  an `assert_eq!` on two `f64`s one ulp apart, red on `main` as well; the
  eighth wave's stream J filed the fix in `docs/open-findings.md` ("a float
  compared exactly").
- `tests/scenario_mem.rs::two_processes_on_one_line_take_it_from_each_other_and_on_two_lines_do_not`
  — "HITM came from" one process of the two, under the load of four streams'
  suites. Rerun alone once: it passed. It is not in the flaky tally by name and
  belongs there: a hardware-sampled cache-line fight is exactly the kind of
  measurement a shared machine takes from.

---

## Index rows

For `examples/README.md` and `GUIDE.md` — the session-index test in
`tests/cli.rs` is red on this branch until they are added:

```
| `triage-latency-cpu.yml` | Why is a cpu-bound program slow? The session `omn latency` asks through: on-cpu dominates, and the hottest function is named. |
| `triage-latency-io.yml` | Why is a program making synchronous writes slow? Its blocked time is uninterruptible, from fdatasync: the disk. |
| `triage-latency-lock.yml` | Why is a thread taking a contended mutex slow? Its sleep is covered by pthread_mutex_lock waits, and the mutex is named. |
| `triage-latency-tail.yml` | What makes the slowest of five runs slow, when the median is cpu-bound? The one run's 200 ms sleep. |
| `triage-saturate-queue.yml` | What do four threads on one cpu queue for? The cpu, three deep, by Little's law over `sched.runqlat`. |
```
