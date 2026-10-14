# Triage — the layer above the engine, and the escalation

`ROADMAP.md` phase 10: the tenth wave's stream T built the three questions,
and the eleventh wave's stream E the escalation, item 3. This is the
deliverable, not a log about one: what the layer is, what it answers and
against what ground truth, what it does not do and why, what it asked of the
files it does not own, and the rows the coordinator pastes.

```console
omn latency  --objective latency|tail        [--runs N] [--pid PID -d DUR | -- PROGRAM]
omn saturate --objective throughput|tail|energy          [--pid PID -d DUR | -- PROGRAM]
omn diff BEFORE AFTER [--format text|json|folded]
omn -f SESSION          # a session with `escalate:` decides which layer to pay for
omn SOURCE --window START..STOP ...
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
| `src/triage/escalate.rs` | the escalation's policy: a reading of the cheap pass, and which layer it names |
| `src/collect/mod.rs` | `Escalate`, the trait the runner asks between rounds, and the held slots |
| `src/request.rs` | `window: on <layer>`, `escalate: {objective}`, and their refusals |
| `src/triage/workload.c` | the ground-truth fixture every scenario launches |
| `tests/triage.rs` | 23 scenarios: ground truth, refusals, the loss path, the capability sweep, the escalation |

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

A launched program that failed -- exited with a status other than zero, or
died of a signal the run did not send it -- is said as a warning under the
verdict, and carried as `program_failed` (one line per run; `omn saturate`
carries one). A verdict read off a program that printed its usage and exited
2 decomposes two milliseconds of startup, and read exactly like a real answer
until it said so.

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

## The escalation — pay for one layer at a time

`omn latency` pays for every detailed layer for the whole run -- the on-cpu
profile, the blocking stacks, the lock sources -- because it does not know
ahead of time where the time will go. A session with an `escalate:` block runs
a cheap pass instead, and once the cheap pass names the state a task's wall
clock is going to, attaches that state's detailed sources and lets go of the
rest (`examples/attach-escalate-phases.yml`):

```yaml
escalate: {objective: latency}
requests:
  - {label: blocked, source: sched.offcpu,   target: &p {launch: [...]}, mode: trace, emit: [summary], keys: [task]}
  - {label: on-cpu,  source: cpu.oncpu_time, target: *p, mode: poll@50ms, emit: [summary]}
  - {label: profile, source: cpu.profile,    target: *p, mode: sample@997hz, emit: [stack], stack: fp, window: on cpu}
  - {label: futex,   source: sched.futex,    target: *p, mode: trace, emit: [summary], window: on lock}
  - {label: mutex,   source: sched.mutex,    target: *p, mode: trace, emit: [summary], window: on lock}
```

### The mechanism, which is the decision worth writing down

**Declared, then decided.** A detailed source is a request like any other,
held with a window written `on <layer>`: validated, resolved and *prepared*
before the program is launched. What the run decides is only when each one is
attached and detached. So an escalation can never reach a backend the
resolver did not choose, or a source answering a question the session did not
ask -- the one thing the brief rules out, ruled out by construction rather
than by a check. The layers are the four states thread state analysis splits a
wall clock into: `cpu`, `queue` (runnable), `disk` (uninterruptible) and
`lock` (sleeping).

**The runner asks a trait; triage answers.** `collect::Escalate` has three
methods -- `decide` (what to attach and detach now), `shed` (let an
over-budget layer go rather than end the run?) and `close` (what each
collection should say) -- and the runner knows nothing behind it, the way it
knows `Cheaper`. It asks once a round, *after* the round's reads (the edge
convention: read first, then decide) and after the stop condition, so a run
that is ending attaches nothing. It applies the answer **detaches first**, so
the run never pays for two detailed layers at once. The one line of the
collector layer that names triage is `escalation_for`, the registration, as
`registry()` names the families.

**Why a session and not a subcommand.** `src/main.rs` is stream J's this wave,
and a new subcommand needs a dispatch line there. The session is also the
better surface: `-o`, `--budget`, `cost.json`, the encoders and the resolved
file all come with `omn -f` for free, and the file is the record of what was
asked, as `DECISIONS.md` settled for every other measurement.

**The cheap pass** is two requests the session runs for the whole run, on the
workload: `sched.offcpu` (where the main thread blocked, in which state, and
how long it then queued) and `cpu.oncpu_time` (what the process burned). The
main thread is the one whose id is the process id, as for `omn latency`.

**A decision** (`src/triage/escalate.rs`) reads what arrived since the last
one, every 200 ms -- two drains of `sched.offcpu`. The main thread's time on a
cpu is bounded from both sides the cheap pass has: no more than the process
burned, and no more than what the main thread's own blocks left of the
interval (a helper spinning a whole cpu while the main thread sleeps is not a
cpu-bound main thread; a unit test holds that). A state is **named** when it
holds more than half of the reading and more than any other; a layer is
**escalated to** when the same state is named in two readings in a row, so a
reading straddling a phase change attaches nothing for the phase that ended.

### What it says, each a scenario

Every held request's notes say when it was attached or let go, by the
escalation, with the reading that decided it and the monotonic span of the
attach; `blocked`'s notes carry the decisions. Measured here (the example,
`--budget 5%`):

```
blocked   the escalation, for latency, named `cpu` at 0.450 s, attaching `profile`; then `lock` at
          1.461 s, attaching `futex` and letting go of `profile` (not served: `mutex`: never attached:
          it is served through `preload`, which is given to the program at exec ...)
profile   559 events over 1.01s, stopped because detached 1.461 s into the run by the escalation: for
          latency, the cheap pass named the `lock` layer (over the last 200.07ms, the main thread was
          blocked sleeping 197.92ms (99 %), on a cpu at most 1.61ms (1 %), ...), so the `cpu` layer's
          detailed sources were let go: the run pays for one detailed layer at a time
futex     attached 1.461 s into the run by the escalation ... attaching it took 5.2ms while the target ran
budget: [cpu.profile] --budget 5% held: 2.31 % measured
budget: [sched.futex] --budget 5% held: 0.89 % measured
```

- **What it escalated to and why** -- the verdict, the objective, the moment,
  the sources attached and let go: the ground-truth scenario asserts each
  field. A run that decided nothing is a different document: a 150 ms program
  (`a_run_too_short_to_decide_attaches_nothing_and_says_so`) says "named no
  layer", and every held request "never attached: the escalation did not name
  its layer", with no events and no cost.
- **The budget holds, measured**
  (`an_escalating_run_holds_its_budget_and_names_what_it_let_go`): every
  collection in `cost.json` is under the ceiling or was let go for exceeding
  it and says so, and the run's whole cost -- every collector's cpu against
  the program's -- was **0.71 % and 0.73 %** of a 5 % budget in two runs. What
  it let go to stay there is named: the profile, at the switch. A layer over
  its own ceiling with nothing cheaper in its chain
  (`a_detailed_layer_over_its_budget_is_let_go_and_the_escalation_goes_on`: a
  20 kHz profile under 0.2 %) is let go "to hold the budget", the run goes on,
  and the escalation still names `lock` afterwards.
- **A layer this machine cannot serve**
  (`a_layer_this_run_cannot_serve_is_reported_and_the_cheap_answer_stands`):
  on a launched program `sched.mutex` resolves to the preload shim, given at
  exec, and glibc's probes are refused for a launch by the catalog -- so this
  run cannot serve that source mid-run, which is real rather than simulated.
  Named, the lock layer is reported "could not be served", with the reason,
  and "the cheap answer stands, and it is only what the cheap pass can say:
  ... blocked sleeping ...". Nothing is attached in its place. The cpu layer is
  let go all the same, since it is no longer the suspect.
- **Joins across the switch are honest** by refusal, until J's windows land:
  a join or a decomposition that reads a held source is refused at load
  ("the join engine does not yet know when a source was watching"), and the
  cheap pass, which runs for the whole run, can be joined freely. See below.

### Ground truth: cpu-bound for a second, then lock-bound for a second

`workload phases 1000 1000 2000 LOG` spins a second of its own cpu, stamps the
switch on `CLOCK_MONOTONIC`, then takes a mutex a helper holds 2 ms at a time a
thousand times, writing every acquisition's start and wait to `LOG` on the same
clock. The tool reads none of it.
`a_program_cpu_bound_then_lock_bound_escalates_to_the_lock_layer_and_its_waits_match_its_own_clock`
asserts: the profile is live before the switch and named `cpu`; `sched.futex`
is attached after the switch; the profile was closed after the switch and
before the futex attach began; and the futex waits measured for the main
thread lie between the program's own waits that started once the futex
program was live (less 5 % and 2 ms, the compare-and-swap before each wait)
and those that started from the moment the attach began (plus one hold, a
call straddling the attach). Three runs:

```
lock layer attached   program waited, from live .. from attach   futex measured
  +463.7 ms              531.47 .. 535.41 ms                        532.74 ms
  +406.3 ms              602.45 .. 606.39 ms                        603.67 ms
  +421.5 ms              592.47 .. 596.38 ms                        591.96 ms
```

The lock layer is attached 0.4 to 0.5 s after the switch: two readings of
200 ms, the first of them straddling it.

### Refused before anything runs

Exit 2, the validator's code, before the launch
(`an_escalation_that_cannot_decide_anything_is_refused_before_the_launch`): a
request held with no `escalate:` block; an `escalate:` block with no
objective (the list), or with `throughput`, `tail` or `energy` (each names the
command that answers it); a session with no cheap pass; a join across a held
source; and `--window on lock` on the command line, which has no cheap pass
beside it.

### What the escalation does not do, and why

- **One window per request.** A layer named, let go, and named again is not
  attached a second time: a collection has one window, and two would be two
  series under one label. The decision says so. The flight recorder (next
  wave) is where a request that comes and goes belongs.
- **Only `latency`.** The tail takes several runs; throughput and energy are
  about every task, and the cheap pass reads one task's wall clock.
- **A held request that does not resolve refuses the run**, because `main.rs`
  resolves every request before running and stops at the first failure. The
  request that makes it a reported layer instead is filed with J.
- **The on-cpu reading is the process's**, bounded by the main thread's own
  blocks: `cpu.oncpu_time` through `/proc/<pid>/stat` has no per-thread
  figure, and its bpf backend is not built. Its tick granularity (10 ms) is in
  every reading, which is one reason a reading is 200 ms.
- **An attach mid-run is charged to its request**, as hot attach decided. Two
  things follow and both are in the runner now. The kernel's symbol table,
  which a profile reads on its first start, took **962 ms** inside the round
  the first time -- every other collector unread, the profile's cost 71 % of
  the program's -- and is now read before the launch when any late request
  asks for stacks. And a request attached mid-run is weighed against its
  budget once it has run for 100 ms: a few milliseconds in, its attach is most
  of its account and a percentage says how recently it was attached. A layer
  attached less than 100 ms before the run ends is never weighed and its
  `cost.json` can show it over the ceiling (seen once: 6.46 % over 93 ms, 5.2
  ms of it the attach), which the budget line reports as exceeded.

### Joins avoided

None of the escalation's sessions joins a held source, and the session
refuses one: `Session::check_escalation` names the join or decomposition part
that reads it. Stream H measured what such a join does today -- a state
opened before the detach absorbs every event after it -- and J's
`apply_within` is the fix. The refusal is one function to lift for joins once
it lands and `main.rs` passes the windows; a decomposition part would also need
weighing against its own window.

---

## `--window` on the command line

Stream H's request, filed with `src/cli.rs` last wave: `omn SOURCE --window
1s..3s` builds the same `Request` a session's `window: 1s..3s` does, through
the same `FromStr`, so the two refuse the same things in the same words (a
unit test compares the refusal texts). `a_window_on_the_command_line_is_attached_and_detached_as_a_session_s_is`
in `tests/runner.rs` runs one: a 200 ms window over a one-second program,
detached where it was asked and watched for its window only. `man/omn.1` has
the option.

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
- **No `--budget` on `omn latency` or `omn saturate`.** A triage question
  pays for the sources its method needs and measures what they cost on every
  result. The escalation is where a budget decides something, and it runs
  through `omn -f`, where `--budget` and the ladder already are.

---

## The manual

`man/omn.1`, this wave: `--window` under *Measurement*; a *Windows and the
escalation* section under *SESSION FILES* (`window`, `on <layer>`, the
`escalate:` block, one window per request, the refusal of joins across a held
source); *The escalation* under *TRIAGE* (the reading, the naming rule, the
release order, the unservable layer, the budget); and the example. The tenth
wave's changes are as that wave left them: the three commands, the `TRIAGE`
section, `EXIT STATUS` and `FILES`.

---

## Filed in `docs/open-findings.md`

Under *Needs from another stream — the eleventh wave, stream E (the
escalation)*:

1. `src/main.rs` (J): a held request that does not resolve is set aside with
   the resolver's reason rather than refusing the run, so the escalation can
   report the layer with the probes' reasons when it names it.
2. `src/join.rs` (J): once `apply_within` lands, lift the refusal of joins
   across a held source in `Session::check_escalation`.
3. `src/emit.rs` (J): the window on `Results`, seconded -- the escalation's
   windows are in its notes, with monotonic stamps, until it is a field.
4. The coordinator: the escalation's mechanism for `DECISIONS.md`, and the
   index rows below.

The tenth wave's five requests are still under their own heading, *the tenth
wave, stream T (triage)*.

---

## Running it

```console
make                                  # capabilities; the eBPF sources need them
cargo test --test triage              # builds the fixture, runs the 23 scenarios
W=target/omn-triage-fixture/workload
omn latency  --objective latency -- $W lock 150 2000
omn latency  --objective tail --runs 5 -- $W tail /tmp/counter 5 100 200
omn saturate --objective throughput -- $W queue 4 250
omn latency  --objective latency --dump-session -- $W io 300 target | omn -f - --input-format yaml
omn -f examples/attach-escalate-phases.yml --budget 5%
omn mem.rss -m poll@20ms -e summary --window 200ms..400ms -- sleep 1
```

---

## The suite on this branch

One `make check`, red at `tests/cli.rs`, then one `cargo test --workspace
--no-fail-fast`: 1762 passed, 9 ignored (the bench's), 4 failed -- one
expected, three that passed alone on their one rerun.

- `tests/cli.rs::the_session_indexes_hold_exactly_the_sessions_on_disk` -- the
  one session below, until its rows are pasted. Expected.
- `tests/triage.rs::the_same_program_is_cpu_bound_for_latency_and_sleep_bound_for_the_tail`
  -- the median of five runs was named `cpu_queue`, "runnable 180.75ms (63.4 %
  of its wall clock)": the machine's cpus were busy with the other streams'
  builds, and the test asserts `cpu`. This stream's file and the tenth wave's
  scenario; nothing this wave changed is on its path (`omn latency` runs no
  escalation). It asserts what the machine was doing, and belongs in the
  flaky tally beside finding 41's kind, with a skip on the fixture's own
  run-queue share the way the io saturation scenario has one -- left for a
  quiet look rather than patched under load.
- `tests/cli.rs::a_known_allocation_shows_up_in_the_measured_series` --
  "touched 67108864 bytes but the measured rise was only 17391616". Not this
  stream's; passed alone.
- `src/perf.rs::tests::an_ibs_sample_says_which_level_answered_the_load` --
  "too few samples to say anything" (7 loads of the 256 MiB walk sampled).
  Not this stream's; passed alone.

`cargo test --test triage` is 23 tests in about 40 s, `--test runner` 21 and
`--test budget` 12; `clippy -D warnings` is clean. `cargo fmt --check` is
clean on every file of this stream and red on one that is not:
`tests/scenario_runtime.rs:926`, the `=~` assertion, is not formatted on
`main` either, so `make check`'s last step fails there on any branch. A
workspace-wide `cargo fmt` rewrites it; this stream's commit had it and took
it back out. Filed for the coordinator.

---

## Index rows

For `examples/README.md` and `GUIDE.md` -- the session-index test in
`tests/cli.rs` is red on this branch until they are added. The tenth wave's
five `triage-*` rows are in both indexes already.

```
| `attach-escalate-phases.yml` | Can a run decide which layer to pay for from its own cheap numbers? A program cpu-bound then lock-bound gets the profile, then the futex waits, one layer at a time. |
```

## Ignored tests this half makes ready

None. The two ignored scenarios in `tests/runner.rs` wait on stream J's join
windows and stream S's per-task counters, and this stream did not touch them.
