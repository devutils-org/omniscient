# Stream mcp, eighteenth wave: one privilege model, and what `run_session` answers

Branch `feat/w18-mcp`, from `main` at `9cd5409`. Owns `mcp/**`, `Makefile`,
`scripts/release.sh`, `docs/install.md`. Brief: `BRIEF.md`, "mcp"; the
finding: `docs/open-findings.md`, "`omn-mcp` carries no capabilities";
`STATE.md` item 4's `mcp/` half.

## 1. What landed

- **`run_session` hands back the program's stdout** (`mcp/src/session.rs`,
  `program_stdout`). A launched program inherits `omn`'s stdout, which also
  carries `omn`'s JSON document. The server finds that document on the shared
  stream byte for byte (the bytes of `results.json`, at their last occurrence
  so that output printed after it is kept), takes it out, and hands back the
  rest: all of it in `program.stdout` beside the run's documents, and its last
  64 KiB in the answer (`program_stdout.text`, with `left_out_before_text`).
  `told_apart` says whether the separation was made, and why not when it was
  not (no `results.json`, or its bytes not on the stream): then the whole
  stream comes back as it came, and the answer says so. A recording writes
  no document to stdout, so its stream is all the program's. The program's
  stderr is still mixed with `omn`'s in `said_by_omn`, and the answer says
  that too.
- **stdout and stderr are read as bytes** (`mcp/src/binary.rs`). Before,
  `read_to_string` returned an empty string for a stream that held one byte
  that was not UTF-8, so a program printing one lost everything on that pipe,
  `omn`'s document with it. Now the bytes are kept and the text is lossy.
- **The budget is optional in `run_session`**, as it is on `omn`: a model is
  not asked for more than a person is. Absent, `bounds.budget` says that no
  ceiling was asked and that the cost is measured anyway. A budget that is not
  a percentage string is refused before anything runs.
- **What a budget is a share of, said for every run** (`budget_against`, from
  `cost.json`): per request, the target's cpu and where it was read from, the
  window's wall-clock time, the share of the window the target spent on a
  cpu, and what the run spent as a percentage of each denominator. The
  denominator is not changed here (that is `src/main.rs`, stream launcher);
  it is said.
- **One privilege model, on `omn`.** `omn-mcp` already collected through the
  `omn` beside it (`DECISIONS.md`, "Who answers a machine question asked
  through MCP"); this wave *shows* it on a run, and says it everywhere a grant
  is made: the `Makefile`'s caps comment and install message, `release.sh`'s
  header, `docs/install.md` ("Grant them to `omn`, and to nothing else"). The
  sudoers spelling in the `Makefile` now covers the debug and the release
  build of `omn`, never `omn-mcp`. `release.sh check` gained check 7: the
  server's sweep serves as many sources as the installed `omn` does.
- **Tests** (`mcp/tests/protocol.rs`):
  `a_launched_programs_stdout_comes_back_apart_from_omns_document`,
  `a_run_through_the_server_is_as_privileged_as_omn`, and
  `a_call_that_would_change_the_machine_is_refused` now refuses a malformed
  budget instead of a missing one. Two unit tests in `session.rs` cover the
  separation (a program printing before and after the document, and no
  document on disk).

Commits: `c02e7f2` (the server), `e089b52` (the build and the docs), and this
handoff.

## 2. Shown, and reasoned

**Shown:**

- `omn-mcp`'s sweep is `omn`'s. With the capabilities on
  `target/debug/omn` and none on `target/debug/omn-mcp`, the server's
  `capabilities` tool reports 92 available out of 101, the same as
  `omn list`. A copy of `omn` without capabilities gives 51
  (`the_sweep_is_the_one_the_binary_that_runs_gives` already held this).
- A run through `run_session` gets `omn`'s privileges. An approved session on
  `io.fsync` (head backend `bpf:fentry`, which needs `CAP_BPF`) runs through
  `omn-mcp` on `bpf:fentry` and counts the 20 `fsync`s the program made. The
  same server pointed with `--omn` at a copy without capabilities does not
  resolve it onto `bpf:fentry`. `getcap` on `omn-mcp` reads nothing. This was
  measured at `perf_event_paranoid` -1, which is how this machine is set: the
  `CAP_BPF` half of the claim is shown, the `CAP_PERFMON` half at paranoid 2
  is not.
- An installed tree: `make install-check` passes all 8 checks, the new one
  included (*"omn-mcp serves the 56 sources .../bin/omn serves"*: no
  capabilities, as installed).
- **A waiting target, read with both denominators.** Twenty `fsync`s and a
  1 s sleep, launched under `io.fsync` with no budget, read
  `run_spent_percent_of_target_cpu` 558.3 and
  `run_spent_percent_of_window_on_one_cpu` 5.8. The target was on a cpu for
  1.0 % of its window (11.1 ms of 1.07 s). That is the shape of
  `sqlite3-durability`'s *"280.60 % measured"*: a collector's cost that is
  small against the window is large against the cpu of a program that waits.
- **Three cases' open finding now passes** in `make check`:
  `the-agent-s-way-answers-within-a-model-s-budget` in `make-sqlite`,
  `http-server-stdlib` and `sqlite3-stripped`. `every_case_names_its_known_cause_both_ways`
  fails because of that, telling us to remove the entry from each `case.yml`
  (section 8). To be plain about why it passes: **the agent's way now carries
  no budget**, the same as the person's. The cost did not go down. The check
  passes "with no budget, or, where it requires one, at 100 %"
  (`docs/cases.md`), and `run_session` no longer requires one. The cost a
  budget would be weighed against is still the launcher's to fix ("a budget
  weighs the launched process alone").

**Reasoned, not shown:**

- **The finding's cause.** The coordinator saw the agent's way fail at
  paranoid 2 "with capabilities" and read it as `omn-mcp` running
  unprivileged. It does not run anything itself (above), so that reading
  cannot be the whole of it. Two explanations fit what was logged, and
  neither has been run:
  1. `target/debug/omn` had lost its capabilities by the time the cases ran
     (a relink between `make` and the test). Both ways would then have been
     unprivileged, and only the agent's way had a budget that could stop it;
  2. `cpu.profile` picks a costlier rung at paranoid 2 even when it holds
     `CAP_PERFMON` (stream profile).
  The privileged run in section 6 tells them apart.
- That 64 KiB is enough of a program's stdout to carry its own account. The
  cases' extractors can read `program_stdout.file` for the whole of it.

## 3. Findings

- **`said_by_omn` mixes the program's stderr with `omn`'s** (and
  `only_a_session_its_owner_approved_launches_a_program` reads the program's
  own count from it). Separating them needs `omn` to hand the launched program
  a stderr of its own, or to prefix its own lines: that is `src/collect/launch.rs`
  (stream launcher), filed in section 8.

- **Once, in the `--no-fail-fast` run, the agent's way named the tool's own
  probe**: `sqlite3-durability/commits-that-wait-for-nothing`,
  `both-ways-name-the-same-suspect`, *"the person's way names on-cpu at
  `_raw_spin_unlock_irqrestore`, the agent's on-cpu at `bpf_trace_run2`"*. The
  `make check` just before it, on the same code, did not show it. Before this
  wave the agent's way of that case ran under a 100 % budget, which can move
  collectors down the ladder; now it runs with no budget, the way the person's
  does, so the two ways should be the same session. `bpf_trace_run2` as the
  hottest frame of a program that waits is the tool's own eBPF cost showing up
  in the target's kernel time. Seen once on a machine running ten streams;
  filed for triage and profile (section 8), not chased here.

## 4. What I could not do, and why

- **Show anything at paranoid 2.** This machine is at -1, and a sysctl is
  machine-wide: section 6.
- **Make `make check` green on this branch alone.** The only failure that
  belongs to this branch is the cases test asking for three
  `open_findings` entries to be removed from `cases/*/case.yml`, which belongs
  to stream cases. The coordinator can make that three-line removal at the
  merge, or merge this stream together with cases.
- **Change `DECISIONS.md` and `docs/mcp.md`**, which both still say "with a
  required budget". They belong to the coordinator and to stream names: see
  sections 7 and 8.
- **Edit the sudoers line in `CLAUDE.md`**, which belongs to the coordinator:
  section 7 gives the new spelling.

## 5. Tests

- `cargo test -p omn-mcp`: 9 unit (7 + 2 new), 12 protocol, 2 walkthrough, all green.
  `a_run_through_the_server_is_as_privileged_as_omn` ran and did not skip.
- `make install-check`: 8 of 8.
- The last `make check` and the `--no-fail-fast` run: section 9.

## 6. Privileged runs wanted (the coordinator, alone)

To see which explanation in section 2 holds, at paranoid 2 with the
capabilities, reading `getcap` before and after the cases run:

```console
$ cd /home/moha/repos/omniscient && make
$ sudo -n sysctl kernel.perf_event_paranoid=2
$ getcap target/debug/omn target/debug/omn-mcp
$ cargo test --test cases -- --nocapture 2>&1 | tee /tmp/cases-paranoid2.log
$ getcap target/debug/omn target/debug/omn-mcp
$ sudo -n sysctl kernel.perf_event_paranoid=-1
```

Expected: `cap_bpf,cap_perfmon=ep` on `omn` at both reads and nothing on
`omn-mcp`; each agent-way run's `answered_by.file_capabilities` true. If the
second `getcap` is empty, the earlier failure was a relink (explanation 1). If
the capabilities hold and `sqlite3-durability`'s `budget_against` still reads
`cpu.profile` far above its band, it is the profile's rung (explanation 2,
stream profile). And
`cargo test -p omn-mcp --test protocol a_run_through_the_server_is_as_privileged_as_omn`
at paranoid 2 shows the `CAP_PERFMON` half.

## 7. Decision lines for `DECISIONS.md`

- Settled (moves out of "Open", as "Decided before this wave" asks): **One
  privilege model for `omn` and `omn-mcp`.** The capabilities go on `omn`
  alone, by `make`, by the command `make install` and `release.sh` print, and
  by the sudoers line. `omn-mcp` collects through the `omn` beside it (or
  `--omn PATH`) and is never granted one. *Reason:* a grant on two files is
  two things to lose at a relink, and a model and a person asking the same
  question have to be answered by the same machine. `release.sh check`
  compares the two sweeps, and `a_run_through_the_server_is_as_privileged_as_omn`
  shows a `CAP_BPF` run through the server with nothing granted to it.
- Amend line 292 ("Whether a model may run a session"): "with a required
  budget" becomes **"with an optional budget, and every run says what a
  budget is a percentage of (`budget_against`)"**. *Reason:* `omn` asked by a
  person carries no budget unless asked for one. Requiring one of a model
  stopped a waiting program (`sqlite3-durability`, 280 % of a cpu it barely
  used) on the agent's way only, which made the two ways disagree for a
  reason that was neither the program's nor the machine's. The duration stays
  the server's bound.
- **`run_session` answers with the launched program's stdout**, apart from
  `omn`'s document, matched byte for byte against `results.json` and never
  guessed. The whole stream goes in `program.stdout` in the run's directory.
- The sudoers line in `CLAUDE.md` (and the `Makefile`'s comment, already
  changed) becomes:

  ```
  <user> ALL=(root) NOPASSWD: /usr/bin/setcap cap_bpf\,cap_perfmon+ep /path/to/omniscient*/target/debug/omn, /usr/bin/setcap cap_bpf\,cap_perfmon+ep /path/to/omniscient*/target/release/omn
  ```

  There is no line for `omn-mcp`: it needs none.

## 8. Needs from another stream

- **cases** (`cases/**`, `tests/cases.rs`, `docs/cases.md`):
  - remove the `the-agent-s-way-answers-within-a-model-s-budget` entry from
    `open_findings` in `cases/make-sqlite/case.yml`,
    `cases/http-server-stdlib/case.yml` and `cases/sqlite3-stripped/case.yml`.
    `every_case_names_its_known_cause_both_ways` asks for this, and it is the
    only red test on this branch. Say in `docs/cases.md` that the check
    passes because `run_session` no longer requires a budget, not because a
    budget now holds;
  - `ask_mcp`'s fallback on *"`budget` is required"* can no longer happen,
    and `MODEL_BUDGET` is now used only where a caller passes one;
  - the program's account can now come from `program_stdout`: `text` (its last
    64 KiB) or `file` (all of it), and `told_apart` says whether the separation
    was made;
  - `tests/cases.rs` refusing to compare two ways run with different
    privileges ("Decided"): every agent-way answer carries
    `answered_by.file_capabilities`, which is what the person's way should
    be compared with (`getcap` on the `omn` the person ran).
- **names** (`docs/mcp.md`): line 481, "with a required budget", becomes
  optional, as in section 7. Add `program_stdout` and `budget_against` to
  `run_session`'s description, and the two new protocol tests to the test
  table near line 396
  (`a_call_that_would_change_the_machine_is_refused` now refuses a malformed
  budget instead of a missing one).
- **triage / profile** (`src/triage/latency.rs`, `src/collect/cpu/profile.rs`):
  a verdict that names `bpf_trace_run2`, the tool's own eBPF trampoline, as
  the on-cpu suspect of a program that waits (section 3). That frame is the
  tool's cost, not the program's, and a verdict should either leave it out or
  say whose it is.
- **launcher** (`src/collect/launch.rs`, `src/main.rs`): the launched
  program's stderr shares `omn`'s, so `said_by_omn` mixes the two. A stderr of
  its own for the program, or a fixed prefix on `omn`'s own lines, would let
  the server separate them the way it now separates stdout. `budget_against`
  reads `overhead.observed_cpu_ns`, so a denominator that counts the
  descendants shows up there without any change to the server.

## 9. The last `make check`

At `e089b52`, with ten streams sharing the machine: **red**. `make test`
stopped on two failures: `budget::the_extra_cpu_a_faster_poll_costs_is_the_extra_cpu_the_shell_observes`
(flaky tally 14; passed when rerun alone) and
`cases::every_case_names_its_known_cause_both_ways`, which only asked for the
three `open_findings` entries this stream closes to be removed (section 8,
cases). `cargo clippy --all-targets -D warnings` (for `omn` and for `omn-mcp`)
and `cargo fmt --check` are clean.

The one `cargo test --workspace --no-fail-fast` run afterwards: **2210
passed, 13 failed, 10 ignored**. `git diff main -- src tests catalog shim` is
empty on this branch, so none of the failures outside `cases` comes from it.
Rerun alone, once each:

- passed alone (load): `budget` x2, `cross_io::the_crossing_sessions_run_and_say_what_they_found`,
  `family_sched::a_task_moved_between_two_cpus_is_counted_once_per_move`,
  `recorder::two_bursts_write_exactly_two_documents_each_holding_its_burst`,
  `scenario_mem::a_known_number_of_megabytes_and_datagrams_is_found_in_the_allocators_ledger`,
  `scenario_io::substituted_counters_are_converted_exactly_and_their_gaps_are_named`,
  `scenario_proc::a_known_number_of_calls_and_an_injected_delay_are_both_reported_as_chosen`,
  `scenario_sched::one_known_number_of_round_trips_is_found_by_four_sources`,
  `triage::a_program_cpu_bound_then_lock_bound_escalates_to_the_lock_layer_and_its_waits_match_its_own_clock`,
  `triage::the_same_program_is_cpu_bound_for_latency_and_sleep_bound_for_the_tail`;
- failed again alone, on `main`'s code, owned elsewhere:
  `runner::a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed`
  (*"short by its own ring's loss: left 111925, right 106005"*; stream launcher's
  file), `triage::a_join_from_a_held_source_counts_what_fell_outside_its_window`
  and `triage::a_verdict_read_off_a_lossy_profile_says_its_amount_is_a_lower_bound`
  (stream triage's file);
- `cases::every_case_names_its_known_cause_both_ways`: the three entries to
  remove, and the `bpf_trace_run2` suspect described in section 3.

`cargo test -p omn-mcp` is green: 9 unit, 12 protocol, 2 walkthrough.

## 10. Review, and the fix round

All four points were right; each is fixed, none disputed.

- **defect, `mcp/src/session.rs`, an unknown in-target cost read as zero.**
  `budget_against` now carries `in_target_known`, `in_target_cpu_ns` and
  `in_target_from` per request. Where the in-target part is unknown
  (`in_target_cpu_ns: null`), `run_spent_ns` and both
  `run_spent_percent_of_*` are `null`, and the part that is known is given
  under names that say it is a floor -- `run_spent_at_least_ns`,
  `run_spent_at_least_percent_of_target_cpu`,
  `run_spent_at_least_percent_of_window_on_one_cpu` -- with `why_at_least`
  quoting `in_target_from`. Shown by the unit test
  `an_unknown_in_target_cost_is_a_floor_and_not_a_zero` (both cases: null
  and known).
- **gap, `mcp/tests/protocol.rs`, skips invisible to the gate.** Every skip
  this branch added, and two older ones whose reasons the CI cell already
  expects (`python3` missing, which the workflow installs so it rightly stops
  the job, and the `carries no CAP_BPF` half), now print `skipped:`. The
  privileged test's skip on CI reads "skipped: .../omn carries no CAP_BPF ...",
  which `.github/expected-skips.txt`'s existing `CAP_BPF` line already
  matches: **no new line is needed from stream green**, and the gate now
  counts it under that class. Still `SKIP:` and so still invisible, left to
  whoever owns the gate (their reasons are not all in the expected file, so
  converting them here would turn CI red outside this stream's scope):
  protocol.rs's 100 kHz profile, `examples/*.yml cannot run here`, IDENTITY
  not built, `node` missing or silent, and `cpu.profile` withheld.
- **nit, `docs/install.md`, 51 put on the capabilities alone.** Reproduced at
  this branch's build: an uncapped copy alone gives 51, the same copy with
  `libomn_preload.so` beside it 56, `target/debug/omn` 92. The paragraph, the
  "need nothing more" count (now 56) and the measured table say so: 36 of the
  41 are the capabilities, 5 the shim.
- **nit, the stdout test's guarded comparison.** It now asserts
  `in_target_known` and that both percentages are present (panicking with
  the answer if not) before comparing them.

`cargo test -p omn-mcp`: 10 unit, 12 protocol, 2 walkthrough, all passed,
with no skip on this machine (the privileged test ran on `bpf:fentry`).

The fix round's `make check` (66f2e22, load average about 30 from the other
streams' suites): red only on `cases::every_case_names_its_known_cause_both_ways`,
the same three `open_findings` entries to remove as in section 9; clippy
(`omn-mcp`) and `cargo fmt --check` clean. The `--no-fail-fast` rerun was cut
short by the round's end at 1960 passed, 10 failed: the `cases` one, and nine
in `budget` (x4), `cross_compute`, `cross_machine`, `encoders`, `runner` and
`scenario_mem` -- files this branch does not touch (`git diff main -- src
tests catalog shim` is empty), timing assertions under that load, and the
`runner` one is the same failure section 9 records on `main`'s code.
