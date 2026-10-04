# Stream triage, twentieth wave: what the re-asked cases found

Branch `feat/w20-triage`, from `main` at `444900b`. `BRIEF.md`, "triage --
what the re-asked cases found (item 6)"; `STATE.md` item 6;
`docs/fixes/w18-cases.md` section 3 (3.1, 3.2, 3.5, 3.6) and
`docs/fixes/w18-triage.md` section 7. Files touched: `src/triage/latency.rs`,
`src/triage/diff.rs`, `src/triage/workload.c`, `src/triage/serve.py` (new),
`tests/triage.rs`.

## 1. What landed

| Commit | What |
|---|---|
| `ede15d8` | one spelling of a limit; the tool's eBPF frames never the suspect; a thread blocked on a connection named beside the block |
| `066e816` | a child followed for a job's wait carries the rest of the job |
| `4b4bd9f` | a lossy row that could overtake the largest undecides it; the workloads for the job, the slow client and the lossy verdict |
| `de07e7e` | every answer in `tests/triage.rs` is checked: no eBPF frame named as the hottest function |

- **One spelling of `cpu-queue` and `other-block`** (w18-cases 3.6).
  `Limit` serializes as kebab-case, so the JSON spells a limit the way
  `Limit::as_str`, the text and the catalog do. The snake-case spelling
  (`cpu_queue`, `other_block`) is still accepted when reading, so older
  answers still load. `tests/cases.rs` already turns `_` into `-` before
  comparing, so it keeps passing, and its `LIMIT_SPELLED` check should now
  pass as well.
- **The tool's own eBPF frames are never the named suspect** (w18-cases
  3.5). `diff::hottest` sets aside any sample whose stack has a frame where
  the kernel was running an eBPF program (`diff::is_ebpf_frame`:
  `bpf_prog_*`, `bpf_trace_run*`, `__bpf_trace_*`, `__bpf_prog_*`,
  `bpf_trampoline_*`, `trace_call_bpf`). Those samples are counted and named
  as the tool's own cost (*"; 91 more sample(s) ran eBPF programs -- the
  tool's own cost on the program's cpu -- and are not ranked"*), and they
  are never ranked. A profile made only of such samples names no function.
  The whole stack is checked, not just the leaf, so a helper under the
  program (`_raw_spin_unlock_irqrestore` under `bpf_trace_run2`) is set aside
  as well.
- **A thread blocked beside the main thread** (w18-cases 3.2,
  `http-server-stdlib`'s slow client). There is a new `Relation::Waiting`
  (`"waiting"` in the JSON). It applies when no thread, child or unplaced
  task ran in the main thread's block. If another thread of the same process
  was blocked on a connection for at least a quarter of that block, the
  triage follows that thread and decomposes it beside the block, the same
  way it treats a busy thread (its whole is what its rows account for, and
  its silent rows count as zeros). "On a connection" means the off-cpu stack
  goes through a send or a receive (`latency::is_connection_frame`): a
  `send`/`recv` family name in the C library's spellings, a `sendall`, or a
  frame in Python's socket module (`_socket.cpython-...so`). `select`,
  `poll` and `accept` do not count. An idle helper asleep beside the block
  is still not followed (`a_helper_that_slept_beside_the_block_...` is
  unchanged).
- **A job is a chain of processes** (w18-cases 3.1, `make-sqlite`,
  `gcc-shell`). Partly done: see section 3. When a wait for a child is
  followed, `Followed` now has a `job` field (`Job { tasks, on_cpu,
  runnable }`). It lists every other busy task that a source placed in a
  process off the way, busiest first, with their on-cpu and runnable time
  added up. The text prints it under the followed reading (*"the rest of the
  job, 2 other busy task(s) off the way: on a cpu ..."*). When the verdict
  is undecided because the wait's rest is unread, the sentence now says that
  rest is in other children, not "in no row". This is the alternative the
  finding offered.
- **The lossy-profile verdict under load, decided** (STATE item 6, last
  clause). The test failed under load because a lossy on-cpu row read less
  than an exact runnable row, so the verdict named `cpu-queue` for a spin
  that the kernel counted as on a cpu. Now, if a lower row is a lower bound
  (its source lost records) and could reach the top row's amount at that
  source's loss rate (`amount / (1 - fraction)`, or without bound when the
  loss is throttling rather than a count), the verdict is undecided and says
  why (*"`runnable` is ..., and `on-cpu` is a lower bound at ...: at its
  source's loss rate (40.0 % of records dropped, run-wide) it could be ...,
  enough to overtake it"*). The test reads the kernel's `schedstat` figures
  for the median run. It accepts `cpu` as a lower bound, `cpu-queue` only
  where the kernel says so or the two figures are close, and undecided only
  where they are close or for the reason above.
- **`pid` on profile samples.** The triage already places a task using any
  event's `pid`. A new unit test (`a_profile_sample_that_carries_its_pid_places_its_task`)
  shows that a profile-only child becomes `child` instead of `unplaced` once
  `cpu.profile` carries `pid`. Carrying it is the job of `profile.rs`'s
  owner (section 6).

New ground truth, each with a known answer:

- `workload job MS N` (`src/triage/workload.c`): the program waits for a
  driver, and the driver runs N steps one after another, each spinning MS in
  its own process. Each step prints its own `omn-truth` line.
- `src/triage/serve.py MS`: the main thread waits on a pipe while the
  serving thread blocks in `sendall` to a client thread that sleeps MS
  before reading. The serving thread prints its `omn-truth` line. It is
  written in Python because the case is Python, and because a C program's
  `send` shows up as an unnamed libc frame (section 3).

## 2. Shown, and reasoned

**Shown** (with the capabilities `make` grants, at load average 14 to 21
with the other streams running):

- `cargo test --lib triage`: 72 passed. New tests: the limit's spelling,
  read both ways; the slow-client thread named beside the block and a short
  block on a connection not named; connection frames; the job field and the
  undecided sentence; a profile `pid` places its task; a lossy row
  undecides; in `diff.rs`, the eBPF samples set aside, using the counts from
  `sqlite3-durability` (57 + 34 tool samples against 40 of the program's).
- `cargo test --test triage`: 29 of 29 passed on the last run. The new
  tests:
  - `a_job_s_wait_carries_every_step_of_the_job_beside_the_child_followed`:
    3 steps of 150 ms. The wait is followed into a step, the two other steps
    are in `job.tasks`, and the followed step's on-cpu plus `job.on_cpu`
    agrees with the three steps' kernel figures (lower bound: a quarter
    below; upper bound: the driver's cpu and three periods above).
  - `a_thread_blocked_on_a_slow_client_is_named_beside_the_main_thread_s_wait`:
    `relation` is `waiting`, the decomposed task and the verdict's task are
    the serving thread, the verdict is `sleep`, and its sleep agrees with
    its wall clock minus its `schedstat` figures. By hand, the text reads:
    *"task:… was `blocked: sleeping` 408.63ms (92.8 % of its wall clock)
    while no thread ran in it, and beside it task:…, a thread of the process
    blocked on a connection through it -- the request it serves -- is
    decomposed: ... blocked: sleeping 404.43ms ...; leaf first:
    libc.so.6+0xa08f2 <- _socket.cpython-314-x86_64-linux-gnu.so+0x10140
    <- ..."*.
  - `a_verdict_read_off_a_lossy_profile_...` passed three times alone and
    once in the full file, with the rule above.
  - Every answer `tests/triage.rs` gets is now checked: no
    `hottest function `bpf_...`` (or the other eBPF frames). Held across all
    29 tests.
- `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo fmt --check`: clean.

**Reasoned, not shown:**

- That the re-asked cases (`make-sqlite`, `gcc-shell`,
  `http-server-stdlib`, `sqlite3-durability`) now agree. I did not run
  `tests/cases.rs` (not my file, and the docs stream re-runs the cases in
  phase B). For `http-server-stdlib` I expect the verdict to follow the
  serving thread, since the Python frames match the shape shown above. For
  `make-sqlite`, the verdict still decomposes one `cc1` (section 3), so
  `every-cited-number-agrees` on the job's cpu should still fail unless the
  case reads `followed.job`.
- The loss-rate bound in the lossy rule assumes the run-wide fraction
  applies to this row. The drops may all belong to one ring.
  `Results.by_ring` carries per-ring losses now, and reading the followed
  task's own ring would tighten the bound. I did not do that.

## 3. Not done, and why

- **The job as one decomposition.** The finding's first option was to have
  the job (the child and every process under it, one after another) take
  the wait's place as a single reading. That needs `read_table`,
  `split_blocked` and `hottest` to work over a set of tasks, and the
  blocked row then counts a driver's own wait for its child twice. Without
  `ppid` on any event, "the processes under it" can only mean "every process
  off the way", which `Job` already reports. What I landed is the second
  option, plus the job as a field. Wanted later: either `ppid` on the sched
  sources' events, so the job is the chosen child's subtree, or a job
  reading whose blocked row leaves out the steps' waits for their children.
- **A C program's thread blocked on a connection is not recognized.** With
  `fp` stacks and libc's dynamic symbols only, a C `send` shows up as
  `libc.so.6+0x...` (glibc's internal cancellable-syscall leaf), and the
  caller frame that named `send` is lost. I tried a C `serve` mode first and
  dropped it: nothing was followed, which is honest but not useful. Two ways
  out: `sched.wakeup` in the latency session (who woke the main thread
  answers who it waited on, with no frame names needed), or `--stack
  fp+leaf` / libc symbols on the off-cpu stacks. Both are decisions beyond
  this stream (section 5).
- **The tail's verdict** (`tail()`) does not use the lossy-row rule yet. It
  compares the excess of two runs, and a row lossy in one run but not the
  other needs its own rule.
- **Late discovery** (w18-triage section 7: a child loses up to 14 % of its
  cpu to being found a tick late). This belongs to the profile, and is left
  to the owner of `profile.rs`.

## 4. Privileged runs wanted

None beyond what `make` grants: everything above ran with `cap_bpf` and
`cap_perfmon` on `target/debug/omn`. For phase B, the docs stream (or the
coordinator) should re-run the cases this addresses, alone:

```console
make && cargo test --test cases -- make-sqlite gcc-shell http-server-stdlib sqlite3-durability --nocapture
```

## 5. Decision lines for `DECISIONS.md`

- **The JSON spells a limit as the text does** (`cpu-queue`,
  `other-block`). The old spelling is still read. Leaving it open costs
  every reader a normalization step.
- **A sample in an eBPF program is the tool's cost, never the program's
  hottest function.** It is counted and said. The cost of leaving it open
  is that a durability case names `bpf_trace_run2`.
- **A thread blocked on a connection beside an otherwise empty block is
  followed (`waiting`)**, recognized by its stack's send/receive frames. Not
  decided: whether to add `sched.wakeup` to the latency session so the
  waker names the thread with no frame names needed. That works for C
  programs, but it is one more source in the cheap pass. Cost of leaving it
  open: C servers whose libc frames are unnamed are not followed.
- **A lower row that could overtake the largest at its source's loss rate
  undecides the verdict.** Cost of leaving it open: a lossy profile under
  load names the run queue for a spin.

## 6. Needs from another stream

### To rings (`src/collect/cpu/profile.rs`)

- Carry `pid` beside the task on every `cpu.profile` sample, as the sched
  sources do. The triage reads it with no change needed
  (`a_profile_sample_that_carries_its_pid_places_its_task`), and a child
  only the profile saw is then `child`, not `unplaced`.
- The late discovery: a child that ran 300 ms lost 43 ms (14 %) to *"found
  on a tick after they began"*. The followed task's on-cpu row is short by
  that amount. This blocks `assert_on_cpu_agrees` from using a tighter
  bound.

### To sched (`src/collect/sched/**`, `src/bpf/sched_*`)

- `ppid` on the sched sources' events (at least on the first event of each
  task), so a job's wait can be followed into the chosen child's subtree
  instead of every process off the way.

### To the coordinator (`docs/triage.md`, `DECISIONS.md`, `STATE.md` item 6)

- `docs/triage.md`'s latency section: the new relation `waiting`, the
  `job` field, the lossy-row undecided reason, and the eBPF samples set
  aside in the hottest function.
- Two cases name the 3.6 spelling finding in their notes
  (`cases/gcc-shell/case.yml:172`, `cases/make-sqlite/case.yml:174`); once
  this is merged, the finding is fixed.

### To budget (`docs/mcp.md`)

- One sentence for a model reading a latency answer: a `followed.relation`
  of `waiting` means the decomposed thread was blocked on a connection
  beside the main thread, not running in its block. `followed.job` lists the
  rest of a job whose wait was followed into one child.

### To docs (phase B, `tests/cases.rs`)

- `make-sqlite` and `gcc-shell`: read `followed.job.on_cpu` beside the
  decomposed child's on-cpu when holding the job's cpu
  (`watch.longest_cpu_ns`). The child's row alone is half the job by
  construction.
- The reader's `_` -> `-` normalization of `limit` can be dropped once this
  is merged. `LIMIT_SPELLED` should then hold.

## 7. The suite

- One failure seen in my file and not caused by my change: in the first full
  `cargo test --test triage` (load average 21),
  `a_program_cpu_bound_then_lock_bound_escalates_to_the_lock_layer_and_its_waits_match_its_own_clock`
  panicked with *"the lock layer was never attached"* (`run.attached("futex")`),
  under load. It passed when rerun alone and passed again in the next full
  run. `escalate.rs` was not touched. For the flaky tally.
