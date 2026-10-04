# Stream cases, eighteenth wave: the cases, re-asked (phase B)

`BRIEF.md`, "cases -- the cases, re-asked"; `STATE.md` "After the seventeenth
wave" item 8's `tests/cases.rs` and `docs/cases.md` lines and the privilege
half of "Decided before this wave"; the requests to stream cases in
`docs/open-findings.md` (the seventeenth wave's streams python and build) and
in the phase A handoffs (`w18-triage.md` section 7, `w18-names.md` section 5,
`w18-mcp.md` section 8, `w18-launcher.md` section 6).

Files changed: `tests/cases.rs`, `docs/cases.md`, `cases/**` (every case's
`case.yml` but `cpython-interpreter`'s checks, `cases/shared/watch.py`, and
`cases/held/gcc-shell/` moved back to `cases/gcc-shell/`). Nothing outside the
row.

Every number below is from this worktree at `cc3adc1` (phase A merged) plus
this branch, on the development machine (24 cpus, `perf_event_paranoid` -1,
`target/debug/omn` carrying `cap_bpf,cap_perfmon`, `DEBUGINFOD_URLS` set),
seven runs of `every_case_names_its_known_cause_both_ways` in all.

---

## 1. What landed

### 1.1 The person's way reads the task the verdict names

`read_cli` took `runs[0].states`: the main thread's rows, whichever task the
verdict named. It now reads `runs[median]` down its `followed` chain to the
last reading, the one the verdict decomposes (`Reading::chain` in
`src/triage/latency.rs`), its rows, its `named` lines and its whole; the notes
of every reading of the chain (where names came from is said on the first).
The verdict's `task` is kept and printed on each run's line. A flat
interpreter profile ("the program runs in the python interpreter and its time
is spread ...") names no leaf: only a `named` line that begins `hottest
function` gives one.

### 1.2 The agent's way reads the run the way the triage does

`read_mcp` took the first table's largest row and the heaviest stack of the
whole run, by leaf. It now reads, as `docs/cases.md` "The agent's reading"
writes it down:

- **which task**: from the first table (the task whose rows account for most
  of the life, the main thread), while the table's largest row is `blocked`
  and another task not yet read was busy -- on a cpu or queued -- for a
  quarter of that block, that task's table, the block as its whole; up to
  eight steps (`RAN_IN_IT` and `MAX_FOLLOWED` of the triage);
- **its stack, its own**: a `stack` output adds every task's events up, so
  the approved session asks `raw` beside each `stack`, and the agent reads
  the request whole (`read_run`, `label`, `max_events: 0`) and weighs the
  read task's events only -- a sample's `count` on the cpu, `total_ns` less
  `queued_ns` off it;
- **the perf map** (python finding 1, `w18-names.md` section 5): each sample
  charged to the innermost frame a perf map named -- the maps cited by the
  names note, read from `/tmp` -- or to its leaf;
- **a flat interpreter profile names nothing** (python finding 2): an
  interpreter by a word of the command or by the module of the unnamed frames
  of half the samples, and a hottest function charged by its leaf under half
  the samples.

The run's line prints the agent's path (`followed: task:1488102 blocked
5346.0 ms -> task:1488129`).

### 1.3 Two ways with different privileges are not compared

"Decided before this wave", the half that is this file's. `ask_cli` reads the
capabilities off `target/debug/omn` before it asks (the `security.capability`
attribute, read as `mcp/src/binary.rs` reads it); `ask_mcp` takes
`answered_by` (`binary`, `file_capabilities`) from `run_session`'s answer.
Each is said as the canonical path and the capabilities. When the two differ,
the cause fails `both-ways-run-with-the-same-privileges` with both, and
`both-ways-name-the-same-suspect` is unchecked, "not compared: the two ways ran
with different privileges". On this machine it passed for every cause of every
run: since stream mcp's change both ways run one file.

### 1.4 The format: `says`, `held`, `sometimes`

- **`says:`** beside `never_says` (python's ask): what the person's sentence
  must contain, check `the-sentence-says-what-the-case-knows`. Used on
  `make-sqlite` and `gcc-shell` (`for a child, in \`wait4\``),
  `http-server-stdlib`'s listing (`while other threads of the process ran`)
  and `sqlite3-stripped`'s lock (`a lock polled with a sleep between tries`).
- **`never_says`** is now read against the agent's way too: the frames of the
  stack it names. `http-server-stdlib`'s slow client fails it both ways, the
  same finding.
- **`held:`** (build's ask): a case held back stays in `cases/<name>/`, says
  why and where that was filed, prints `HELD case <name>: <why>`, and is
  still parsed by `every_case_is_well_formed`. No case is held today.
- **`sometimes: true`** on an open finding: a check that alternates from run
  to run (section 3). Its failure prints `FINDING`; its pass prints a `note`
  and asks nothing; it is closed by hand. `docs/cases.md` says it is the open
  finding's form of the flaky tally, and as narrow.

### 1.5 Leaves compared only where the case knows the frame

`both-ways-name-the-same-suspect` compared the two leaves whenever both were
named. On `sqlite3-durability`'s commits, which names no frame, the hottest
leaf is a kernel function at 5 % of the samples and the next at 3.5 %
(`_raw_spin_unlock_irqrestore` 88, `bpf_trace_run2` 57 of 1 646): one run in
four the two ways named different ones, an unlisted failure that was the
profile's noise. The leaves are now compared where the case names its `leaf`;
a cause that names none is about a state. `docs/cases.md` says so.

### 1.6 A stopped run's leftovers, and running fewer cases

- After a budget stop, before the run is asked again, `sweep` ends every
  process whose command line names the case's scratch directory or that works
  under it (not this process or its ancestors) and waits for it (build's ask).
  `omn` now ends the process group itself (`w18-launcher.md` 1.2); this is the
  guard for a program whose children left the group. No run here was stopped.
- `ask_mcp`'s budget path stays as it was, and its comment says why
  `the-agent-s-way-answers-within-a-model-s-budget` passes: `run_session` takes
  no budget unless asked (`w18-mcp.md`), not because a budget now holds.
  `docs/cases.md` says the same in the check's row (mcp's ask).
- `OMN_CASES_ONLY=<case>,...` asks those cases only, and
  `OMN_CASES_KEEP=<dir>` writes every answer as given,
  `<case>-<cause>-<cli|mcp>.json`. Both are in `docs/cases.md`.

### 1.7 The cases

- **`gcc-shell` is back** from `cases/held/` (`git mv`): both ways follow the
  driver's `wait4` into the compiler and name its cause, `on-cpu` for the
  passes, `runnable` for the contended compile, in every run of five. Five of
  its six open findings closed; `every-cited-number-agrees-with-the-reference`
  refiled (section 3.1).
- **`sqlite3-stripped`**: the polled-lock finding closed. The sentence no
  longer says "not a lock: a timer, a pipe or a socket"; `never_says` is now
  that claim (`not a lock:`), and `says` holds the new clause. The case lists
  no open finding.
- **`cpython-perf-map`**: both findings closed -- the agent ranks by the
  perf-map frame and names `py::score:<path>`, as the person does.
- **`cpython-interpreter`**: neither way names a function of the flat profile,
  so the comparison of two C leaves the seventeenth wave's flaky tally counted
  is gone; the comment saying "fragile" is replaced.
- **`http-server-stdlib`**: `the-first-suspect-is-the-known-cause` and
  `both-ways-name-the-same-suspect` closed -- the listing is followed into the
  serving thread both ways and agrees with its `schedstat`. The slow client is
  refiled (section 3.2).
- **`make-sqlite`**: `both-ways-...` and the first suspect closed as findings
  that always failed, and re-listed as `sometimes` (section 3.1).
- **`sqlite3-durability`**: its finding re-measured and listed `sometimes`
  (section 3.3).
- **`cases/shared/watch.py`**: stops at a zombie (a target exited and not yet
  reaped is gone for the account), and its docstring says the stop now ends
  the process group and why a shell that execs is watched by the program it
  becomes (section 3.4).
- **`docs/cases.md`**: the injection table (build's ask) -- the cpu
  contention row is `watch.py`'s two spinners and `taskset -c 0`, a new row
  for a program that is many processes, and `cases/shared/` as the home of a
  helper more than one case calls; the three accounts read through
  `key-value` by a helper of their case (`-ftime-report`, `strace -c`,
  `schedstat`), not registered as extractors of their own; the checks table;
  `gcc-shell`'s row; "The agent's reading".

Two machine-independent tests are new:
`the_person_s_reading_is_the_task_the_wait_was_followed_into` and
`the_agent_s_reading_follows_a_block_into_the_busy_task`.

---

## 2. Shown, and reasoned

**Shown**, seven runs of the case test on this branch (runs 3 to 6 are the
final reading but for section 3.4's revert):

| Case / cause | Person's way | Agent's way | Every run |
|---|---|---|---|
| `sqlite3-stripped` / plan | `on-cpu`, `sqlite3VdbeExec.lto_priv.0` | the same | yes |
| `sqlite3-stripped` / lock | `blocked`, `clock_nanosleep`, through the busy callback | the same | yes |
| `sqlite3-durability` / disk | `blocked`, `fdatasync`, through `sqlite3PagerCommitPhaseOne` | the same | yes |
| `sqlite3-durability` / commits | `on-cpu` | `on-cpu` | yes |
| `make-sqlite` / one job | `on-cpu` in the followed `cc1` | `on-cpu`, followed to `cc1` | 4 of 5 decided (section 3.1) |
| `make-sqlite` / contended | `runnable` in the followed compile | the same | yes |
| `gcc-shell` / passes | `on-cpu` in `cc1` | the same | yes |
| `gcc-shell` / contended | `runnable` in `cc1` | the same | yes |
| `http-server-stdlib` / listing | `on-cpu`, the serving thread, beside the `select` | the same, followed | yes |
| `http-server-stdlib` / slow client | `blocked`, the main thread's `select` | the same | yes, the wrong task (3.2) |
| `http-server-stdlib` / cold file | skipped: storage faster than the copy | skipped | yes |
| `cpython-interpreter` / score | `on-cpu`, no function named | the same | yes |
| `cpython-interpreter` / read | `blocked`, `__read` | the same | yes |
| `cpython-perf-map` / score | `on-cpu`, `py::score:<path>` | the same | yes |
| `cpython-perf-map` / read | `blocked`, `__read`, through `py::wait_for_input` | the same | yes |

So `make-sqlite`, `http-server-stdlib` and `gcc-shell` name their known cause
first, both ways -- `make-sqlite`'s on-cpu cause in four runs of five -- and
what still gets them wrong is filed (section 3). The baseline run on the
merged tree, before this branch's reader, named `blocked` at `wait4` on the
agent's way for both `make-sqlite` causes and `blocked` for the listing.

`both-ways-run-with-the-same-privileges` passed for every cause of every run,
`the-agent-s-way-answers-within-a-model-s-budget` likewise.

**Reasoned, not run**: the privilege refusal on a difference (no run here had
one: both ways are one file now; the unit of it is the comparison of two
strings), and the sweep after a stop (no run was stopped).

---

## 3. Findings

### 3.1 "a job is a chain of processes" -- `src/triage/latency.rs`, `Tasks::follow` and `Tasks::child`

The triage follows a wait for a child into **the busiest process** not on the
way, wherever it is under the program. `make -j8`'s build is the shell's job
-- `sh`, the driver `gcc`, two `cc1` one after the other, `as` -- and the
wait's place is taken by one `cc1`: 2.6 to 2.8 s on a cpu against the job's
5.1 to 5.3 s (`watch.longest_cpu_ns`), its other compile in no row. So:

- `every-cited-number-agrees-with-the-reference` fails both ways on both
  `make-sqlite` causes in every run (on the cpu 2.8 s against 5.2 s; off it
  `runnable` 6.3 to 6.8 s against the job's 6.8 to 7.4 s), and on
  `gcc-shell`'s contended compile (`runnable` 1.67 s against the chain's
  1.81 s: the driver and `as` waited too).
- The person's verdict on `make-sqlite`'s on-cpu cause is **undecided** when
  the followed `cc1` is the shorter half: *"`on-cpu` is 2.640s, and 2.799s of
  the wall clock is in no row that was read (blocked)"*, one run in five here
  (one in three in the first three). Listed `sometimes` on
  `the-first-suspect-is-the-known-cause` and `both-ways-name-the-same-suspect`.

Wanted: the wait's place taken by what ran during it -- the child the wait
names and the processes under it, one after the other -- rather than by its
busiest descendant; or the undecided verdict saying the rest of the wait is in
other children rather than in no row.

### 3.2 "a thread that waits beside the main thread is not followed" -- `src/triage/latency.rs`, `Tasks::follow`

`http-server-stdlib`'s slow client: the main thread's `select` is 2.3 s, the
serving thread's send 1.28 s (its `schedstat`), and the serving thread was
blocked too, not busy, so `RAN_IN_IT` (busy for a quarter of the block) never
follows it. Both ways name the `select`; the sentence and the agent's stack
contain `select.cpython`, and `blocked` is a second over the serving thread's
wait. Listed on `every-cited-number-...` and `the-sentence-claims-nothing-false`.
Wanted: a thread of the process that was alive and blocked through a block of
the main thread whose own block is the program's request -- the one serving a
connection -- named beside it, as a busy one is.

### 3.3 "the durability case's blocked row" -- `w17-build.md`, re-measured

The seventeenth wave filed `blocked` 50 to 60 ms (3 %) under the program's
`schedstat` over 500 commits. Here it is within 7 to 20 ms of it, either
side: over in five runs, under in one, within the 6 ms the two resolutions
allow in one of seven (both ways each time). Listed `sometimes`. Once, in the
first run, the watcher read the agent's run's `schedstat` as zeros for its
whole life (`watch.cpu_ns=0`), not seen again in six runs. What is left over is
partly the reference's: the watcher reads sqlite3 from its exec, and the
shell before it (10 to 20 ms, a copy of the database) is in the tool's life
and not in the watcher's (section 3.4).

### 3.4 The watcher cannot wait for a shell that lives 10 ms

To cover that shell, the case was first watched as `sh`. Two runs of four
then hung on *"the injection did not finish within 90 s of the target"*: a
pass over `/proc` on a loaded machine takes longer than the shell lives, and a
watcher that missed it never finds its target, which is by then `sqlite3`.
Reproduced by hand (one of five short targets found). Reverted to watching
`sqlite3`, the reason in the case and in `watch.py`'s docstring. The watcher
also now stops at a zombie.

### 3.5 "the tool's own programs are the target's hottest function" -- `cpu.profile` and the sched programs

On `sqlite3-durability`'s commits the target's profile holds the tool's own
eBPF programs on its cpu: `bpf_trace_run2` (57 of 1 646 samples, 3.5 %) and
`bpf_prog_..._omn_futex_out` (34), and the person's verdict once named
*"hottest function `bpf_trace_run2`, 96 of 1711 samples (6 %)"*. The tool's
cost is charged to the program and then named as its hottest function.
Wanted: frames of the tool's own programs said as its cost and not ranked as
the program's, or at least never the named suspect. Not a check: it decides
nothing on a cause that names no frame (section 1.5).

### 3.6 The JSON's `limit` is `cpu_queue`, the text's `cpu-queue` -- `src/triage/latency.rs`, `Limit`

`#[serde(rename_all = "snake_case")]` writes `cpu_queue` and `other_block`
where `Limit::as_str`, the text and the cases say `cpu-queue` and
`other-block`. The cases normalise `_` to `-` when they read the verdict.
Wanted: one spelling.

### 3.7 A `stack` output is not keyed by task -- `src/emit.rs`

`keys: [task]` keys the summaries, not the stacks: a stack's row adds up
every task's events. An agent following a wait into a child cannot read the
child's stack from the read-back; the cases ask `raw` beside `stack`. Wanted:
the stack rows keyed by the request's keys, or `docs/mcp.md` telling a model
to ask `raw` when it reads a task other than the main thread.

### 3.8 One flake, not in the tally

`sqlite3-stripped/a-plan-that-scans/cli`'s `every-cited-number-...` failed
once in seven, at a load that stretched the 1.4 s run to 2.1 s: on-cpu
2 127 ms against `.timer`'s 2 088 ms, 39 ms apart against 35.6 ms allowed.
For the tally.

---

## 4. What I could not do, and why

- **The CI's `gcc-shell`**: it is asked again, so the runner needs `gcc`,
  `unzip`, `taskset` and the amalgamation from sqlite.org; whether the cells
  carry them was not checked (`.github/**` is stream green's).
- **The flaky tally's two `cases` lines** are in `docs/open-findings.md`
  ("The seventeenth wave, the coordinator's"), the coordinator's file: filed
  below with the evidence, not removed.
- **Registered extractors** for `gcc-time-report`, `strace-summary` and
  `schedstat-tree` (build's optional ask): not done; the three accounts are
  read through `key-value` by their case's helper, as `docs/cases.md` now
  says.
- **The paranoid-2 runs**: machine-wide; section 5.

---

## 5. Privileged runs wanted (the coordinator, alone)

Every case at `perf_event_paranoid` 2, with and then without the
capabilities, read through the skip gate (`STATE.md` "After the seventeenth
wave" item 1; it failed twice in two runs at `718cae1` because the two ways
ran with different privileges):

```console
$ cd /home/moha/repos/omniscient          # after this stream's merge
$ sudo -n sysctl kernel.perf_event_paranoid=2
$ make && getcap target/debug/omn         # cap_bpf,cap_perfmon=ep
$ OMN_CASES_KEEP=/tmp/cases-p2-caps cargo test --test cases -- --nocapture 2>&1 | tee /tmp/cases-p2-caps.log
$ touch src/main.rs && cargo build --workspace && getcap target/debug/omn   # nothing: relinked
$ OMN_CASES_KEEP=/tmp/cases-p2-bare cargo test --test cases -- --nocapture 2>&1 | tee /tmp/cases-p2-bare.log
$ .github/scripts/skip-gate.sh /tmp/cases-p2-bare.log
$ sudo -n sysctl kernel.perf_event_paranoid=-1 && make
```

What to read: with the capabilities, `both-ways-run-with-the-same-privileges`
`ok` for every cause and no `sqlite3-durability` budget stop; without them,
every cause skipping with the machine's reason, both ways the same reason.

---

## 6. Decision lines for `DECISIONS.md`

- **One privilege model, the cases' half** (moves out of "Open" with stream
  mcp's half): `tests/cases.rs` reads the capabilities of the `omn` the
  person runs and the `answered_by` of every `run_session` answer, and a cause
  whose two ways differ fails `both-ways-run-with-the-same-privileges` and is
  not compared. *Why*: two ways run with different privileges answer from two
  machines; what differs is the machine's, not either way's.
- **The agent's reading follows the triage's**: from the main thread's table,
  a `blocked` largest row is followed into the task busy for a quarter of it,
  with that task's own events for its stack, perf-map frames first, and a
  flat interpreter profile names nothing. *Why*: `docs/mcp.md` promises a
  model can read a run the way `omn latency` does, and the cases are where
  that promise is held; a reading of its own would test a third way.
- **A cause that names no frame compares states, not leaves**. *Why*: the
  hottest leaf of a spread profile moves between two runs, and a comparison of
  two samples of the noise fails one run in four.
- **An open finding may alternate (`sometimes: true`)**, filed with why and
  how often; closed by hand. *Why*: listed, its passing runs fail the test;
  unlisted, its failing ones do; held, the case's other checks stop running.

---

## 7. Needs from another stream

### To the triage's owner (`src/triage/latency.rs`)

- Section 3.1, "a job is a chain of processes" (`make-sqlite`, `gcc-shell`).
- Section 3.2, "a thread that waits beside the main thread is not followed"
  (`http-server-stdlib`'s slow client).
- Section 3.6, one spelling of `cpu-queue` and `other-block` in the JSON.

### To the owners of `cpu.profile` and the sched programs

- Section 3.5: the tool's own eBPF programs ranked as the target's hottest
  function.

### To the owner of `src/emit.rs` (the coordinator: a shared layer), or names (`docs/mcp.md`)

- Section 3.7: stack rows keyed by task, or `docs/mcp.md` telling a model to
  ask `raw` beside `stack` to read a followed task's stack.

### To names (`docs/mcp.md`)

- The reading of a followed task, as `w18-triage.md` section 7 already asked
  ("when the main thread's largest row is a `wait4` ... read the child's or
  the busiest thread's table"): `docs/cases.md`, "The agent's reading", is
  the rule the cases hold the agent to, and `docs/mcp.md` should say the same.

### To the coordinator

- `docs/open-findings.md`, "The seventeenth wave, the coordinator's", the
  flaky tally: **remove both `cases` lines**. The budget line: in seven runs,
  `the-agent-s-way-answers-within-a-model-s-budget` passed on every cause of
  every case (`run_session` takes no budget, and `sched.mutex` is asked only
  in a second pass), and `http-server-stdlib` no longer lists the check.
  The flat-profile line: both ways name no function on
  `cpython-interpreter/one-function-dominates`, and
  `both-ways-name-the-same-suspect` passed in every run (python's finding 2).
- Section 3.8 for the tally.
- `STATE.md` (lines 634 and 691) and `ROADMAP.md` (line 870) still say
  `gcc-shell` is held back under `cases/held/`; it is `cases/gcc-shell/` and
  asked.
- `docs/open-findings.md`'s seventeenth-wave asks to stream cases (python's
  and build's) are landed here, but for the optional extractors.

### To green (`.github/**`)

- `gcc-shell` is asked on the CI again: `gcc`, `unzip`, `taskset`, `python3`,
  and the fetch from sqlite.org (section 4).

---

## 8. The suite

While iterating: `cargo test --test cases` only (the case test seven times,
section 2; the five machine-independent tests each build), `cargo clippy
--tests --test cases -D warnings`, and the case test narrowed with
`OMN_CASES_ONLY` while a case was changed.

One `make check` on `ae7271f`'s tree, with the capabilities (`getcap` before
it): red at `tests/runner.rs`
(`a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed`,
74 246 against 73 363: the throttles `losses_by_ring` does not carry, filed by
stream launcher for stream profile, and red on `main`'s `src/` too per
`w18-engine.md`). `cases` passed in it (6 tests, 194 s).

Then one `cargo test --workspace --no-fail-fast`, written to a log of this
stream's own (see below): **2 271 passed, 5 failed, 11 ignored**; `cases`
green (every case, 193 s). The five, none in this stream's files, each passed
when rerun alone once:

- `collect::mem::page_faults::tests::sampled_in_user_mode_the_weighed_samples_are_the_faults_the_thread_took`
  (in `w18-profile.md`'s list);
- `join_engine::a_program_s_life_is_a_whole_its_three_states_account_for`
  (not in the tally: for it);
- `runner::a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed`
  (above; not rerun, red on `main`);
- `scenario_sched::a_forced_switch_program_splits_the_switches_as_the_kernel_counts_them`
  (in `docs/flaky.md`);
- `scenario_sched::a_mutex_nobody_else_wants_is_held_as_long_and_waited_for_not_at_all`
  (not in the tally: for it).

`cargo clippy --all-targets -- -D warnings` and `cargo fmt --check`: clean.

**One thing for the coordinator about how the wave runs**: the streams'
agents share one scratchpad directory, and a first no-fail-fast run here wrote
`nff.log` there while another stream's run wrote a file of the same name: the
two logs interleaved (two "targets failed" summaries in one file). It was
rerun into a directory of this stream's own. Whoever writes the next brief
may want each stream to log under a directory named after it.
