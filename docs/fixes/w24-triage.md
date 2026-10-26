# The twenty-fourth wave, stream triage

The triage and the cases: the items `docs/open-findings.md` carried for
`src/triage/**` and `tests/cases.rs` since the twenty-first wave (stream
run's "the failure channel" and "`--program-stderr` and `--runs`", stream
data's "the provenance from the results", stream docs' "the job restricted
to the followed child's parent") and the three cases `docs/dev/STATE.md` names.
Branch `feat/w24-triage`, commits `9f3124b`, `f49d539`, `4b101d7`,
`b512089`, the refactor that moves the failures beside the run
(`engine::Ran`, so `tests/view_timebar.rs`, which builds an
`engine::Run`, compiles unchanged), a merge of `main` (stream unwind), and
this handoff. Nothing outside the row was touched: no shared layer, no seam.

## What landed

| Commit | What |
|---|---|
| `9f3124b` | **The failure channel** (defect). `engine::Ran::failures` (beside the `Run`, after the refactor), read off the collections the way `main.rs`'s `failures` reads them (each sentence once, in order); carried by `latency::Answer::failures` (over every run of both passes) and `saturate::Answer::failures`, serialized, and said in both texts as `broken by the tool: <sentence>`. **`--program-stderr` with `--runs`** (defect: each launch truncated the one before). `latency::Launches`: before each launch the file is taken away, after it the file is copied to `FILE.<n>` (`n` counting the launches from 1 across both passes); the file itself keeps the last launch's, as after one run; a launch that wrote nothing takes no number; an attached `--pid` keeps nothing. `Answer::program_stderr` lists the copies; the text says `the program's stderr, run by run: ...`. |
| `f49d539` | **The provenance from the results** (defect: a map read back from `/tmp` can be another process's). `PerfMapNames::of_run` reads which frame a perf map named from each result's `named_by` beside its `stacks`, and which maps from `perf_maps`; nothing is read back when every result with stacks carries `named_by`. Only when one does not (`sched.offcpu`'s, until its provenance lands) are the maps read back -- from the paths `perf_maps` names, else the note's -- and a file whose `(dev, ino)` is not the run's is said as rewritten. `omn diff`'s profile reads the same way. |
| `4b101d7` | **The job restricted to the followed child's parent**, ready for `ppid`. `Job::under`: when the events carry `ppid` beside `pid`, the rest of the job is the processes under the chosen child's parent (that parent included), and the text says `under process N, the child's parent`; without `ppid`, every busy task off the way, as before. |
| `b512089` | **The cases, the agent's way** (`tests/cases.rs`, `read_mcp`). (1) `http-server-stdlib`'s slow client: when nothing busy ran in the block, a thread of the same process (by the offcpu events' `pid`) blocked on a connection (a send or receive on its blocking stack, the same `is_connection_frame` as the person's way) for a quarter of the block is followed *beside* it, its whole its own rows, and no job is added: the person's way's `Relation::Waiting`. Both of the case's open findings close. (2) `sqlite3-durability`'s commits: a `cpu.profile` sample whose stack runs the tool's eBPF programs is set aside and said, never charged, as `diff::hottest` does on the person's way. (3) The perf-map names from the result's `named_by` before `/tmp`. |

## Shown, and reasoned

Shown on this machine (load from the other fifteen streams):

- **The slow client on the MCP way**, `OMN_CASES_ONLY=http-server-stdlib
  cargo test --test cases every_case_names_its_known_cause_both_ways`:
  the agent now reads `task:101776 blocked 2364.0 ms beside, blocked on a
  connection: task:102629`, rows `blocked 1319.6 ms` against the client's
  `handler_off_ns` 1319.5 ms; leaf `libc.so.6+0xa08f2`, the same as the
  person's way. Every check of `a-large-listing` and `a-slow-client` passed
  both ways, and the run went red only because the two `open_findings` now
  pass; they are removed. `a-cold-file` was skipped both ways by its own
  hold (`handler_off_share` 0.11 and 0.06 against at least 0.5: the
  storage was not slower than the copy under this load), so it says nothing
  either way.
- **Per-run stderr**: `omn latency --objective latency --runs 3
  --program-stderr DIR/err.txt -- sh -c 'echo "launch pid $$" >&2; ...'`
  left `err.txt.1`, `.2`, `.3` holding pids 115604, 115727 and 115741, and
  `err.txt` the last (115741); exit 0, the text listing the three.
- Unit tests: `cargo test --lib triage` (78 passed, six new:
  `what_a_collector_broke_reaches_the_run_once`,
  `each_launch_s_stderr_is_kept_under_its_number`,
  `an_answer_says_what_the_tool_broke_and_where_each_stderr_went`,
  `the_results_provenance_names_the_mapped_frames_without_reading_the_map`,
  `stacks_without_provenance_read_the_named_map_back_and_say_another_file`,
  `with_ppid_the_job_is_what_runs_under_the_chosen_child_s_parent`), and
  `tests/cases.rs`'s
  `the_agent_s_reading_names_a_thread_blocked_on_a_connection_beside_the_block`.

Reasoned only:

- **The failure channel end to end**: no collector this machine can serve
  for a triage playbook breaks its program (the GPU's is the one that can,
  and no playbook asks it), so the answer's `failures` is shown by the unit
  tests over real `Collection`s, not by a broken run.
- **The job by `ppid`**: no sched source carries `ppid` yet (stream sched's
  item this wave, not merged when this was written), so `make-sqlite`'s
  critical path still overshoots as filed; the restriction is shown on
  synthetic events only. Once `ppid` lands, `make-sqlite`'s
  `every-cited-number-agrees-with-the-reference` is the check that should
  close, and the case will say so by failing on its open finding.

Three more cases, `OMN_CASES_ONLY=sqlite3-durability,cpython-perf-map,make-sqlite`
(load average about 24):

- **`sqlite3-durability/commits-that-wait-for-nothing`: every check passed
  both ways**, `both-ways-name-the-same-suspect` included (the shape the
  twenty-second wave's coordinator reran alone). `every-commit-waits-for-the-disk`
  passed but for its `sometimes` open finding (blocked 26.5 ms and 24.0 ms
  over the program's `schedstat`, beyond the 8 and 12 ms allowed), as filed.
- **`cpython-perf-map`**: under the load, `one-function-dominates`' person's
  way named `runnable` (`cpu-queue`) and so no stack; **rerun alone once,
  every check passed both ways**, the person's way naming
  `py::score:.../hot.py` through the perf map, the agent's the same through
  the result's `named_by` (a `cpu.profile` result on this build carries
  `named_by` with `perf-map` frames, 26 of them over 13 stacks in a check
  run, and an empty `perf_maps`). Load, not the change.
- **`make-sqlite/the-critical-path-is-one-job`**: its three open findings
  fail as filed (on-cpu 7036 ms person's, 5891 ms agent's, against the
  longest job's 8676 and 8164 ms), unchanged: no `ppid` yet.
  `a-cpu-shared-with-two-competitors` passed every check both ways.

`cargo test --test triage`: 27 of 29 under the load; the two red,
`a_detailed_layer_over_its_budget_is_let_go_and_the_escalation_goes_on`
(in the flaky tally) and
`a_program_cpu_bound_then_lock_bound_escalates_to_the_lock_layer_and_its_waits_match_its_own_clock`,
each passed rerun alone once (4.5 s, 3.9 s). `cargo test --test cases
every_case_is_well_formed` and the two agent-reading unit tests pass;
`cargo test --test view_timebar` passes (it builds an `engine::Run`).
`cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt
--check` clean.

## Not done, and why

1. **The exit code of a broken triage**: `omn latency` and `omn saturate`
   still exit 0 beside `failures`, because the exit is decided in
   `src/main.rs` (stream run's). Filed below; one line each.
2. **`make-sqlite`'s critical path**: waits on `ppid` on the sched sources
   (stream sched). The triage half is in (`Job::under`).
3. **The agent's way's job by `ppid`**: `read_mcp` still adds every other
   task as the job (an upper bound, said in its comment); once `ppid` is on
   the raw events it can read it the way `parents()` does. Not done
   because nothing carries it to test against.

## Review

One finding, a gap, fixed.

- **`Tasks::job`/`descends` dropped a busy process whose own events
  carried no `ppid`** once any event did. Right: a missing parent was read
  as "not under". `descends` now answers yes, no or unknown: yes when the
  parents lead to the chosen child's parent, no when they lead to one of
  the program's own processes first (those are above it) or loop, and
  unknown when they stop at a process no event gave a parent. The unknown
  ones are kept apart as `Job::unplaced` (and `unplaced_on_cpu`), neither
  counted in the job nor dropped, serialized when not empty and said in
  the text as `and N busy task(s) off the way the run did not place under
  it or beside it, their parent unknown, not counted in the job: on a cpu
  ...`. Shown by the partial-coverage half added to
  `with_ppid_the_job_is_what_runs_under_the_chosen_child_s_parent` (no
  real source carries `ppid` yet, so a unit test is all there is to show
  it with). `cargo test --lib triage`: 78 passed; `cargo clippy
  --workspace --all-targets -- -D warnings` and `cargo fmt --check` clean.
  For the coordinator's `docs/triage.md` line: `followed.job.unplaced`
  and `followed.job.unplaced_on_cpu` beside `under`.

## Needs from another stream

### To stream run (`src/main.rs`): exit 1 for a broken triage

In `latency()` and `saturate()`, `Ok(answer) => { print_answer(...)?;
Ok(outcome(&answer.failures)) }` instead of `Ok(ExitCode::SUCCESS)`: the
answer is printed, then each sentence on stderr and exit 1, as `omn -f`
does. `outcome` already exists there.

### To stream run (`src/cli.rs`): `--program-stderr`'s help

One sentence on the flag: "With `omn latency --runs N`, each launch's
stderr is also kept as FILE.1 ... FILE.N, the file itself the last one's."

### To stream sched (`src/collect/sched/**`): `ppid` as an attribute

`latency::parents()` reads an event attribute named `ppid` beside `pid`
(any event, the first seen per pid), so the name is the contract. On at
least each task's first event of `sched.offcpu` or `sched.runqlat`.

### To the coordinator (`docs/triage.md`, `docs/cases.md`, `docs/mcp.md`, `man/omn.1`)

- `docs/triage.md`: the latency answer's `failures` and `program_stderr`
  fields, `saturate`'s `failures`, `followed.job.under`, `followed.job.unplaced`
  and `followed.job.unplaced_on_cpu`.
- `docs/cases.md`, "The agent's reading": the step to a thread blocked on
  a connection beside the block (same process by the offcpu events' `pid`,
  a quarter of the block, whole its own rows, no job), the eBPF samples
  set aside, and the names read from `named_by` first.
- `man/omn.1`: `--program-stderr` with `--runs` writing `FILE.<n>`.
- `docs/open-findings.md`: the twenty-first wave's "To `src/triage/engine.rs`:
  the failure channel" (landed but for the exit, above), "`--program-stderr`
  and `--runs`" (landed: `FILE.<n>`), "To `src/triage/**`: the provenance
  from the results" (landed), "To `src/triage/latency.rs` (`Job`)" (landed
  on the triage side, waits on `ppid`), and the eighteenth wave's "a thread
  that waits beside the main thread" for the MCP way (landed, the case's
  findings removed).

## Decision lines for `docs/DECISIONS.md`

- **Several launches keep each stderr as `FILE.<n>`** (`omn latency
  --runs N --program-stderr FILE`): numbered from 1 in the order the
  launches ran, a second pass continuing the numbers; `FILE` keeps the last
  launch's. Chosen over appending because a run's stderr stays one file a
  reference can be read from, and over `FILE` per run because the last
  run's file is where one run would have left it.
- **The perf-map names are the results' own** where the results carry
  `named_by`; a map is read back from disk only for stacks without
  provenance, and then by the path and `(dev, ino)` the results recorded.

## Privileged runs wanted

None beyond what `make` grants: the cases ran with `cap_bpf,cap_perfmon`
on `target/debug/omn`.
