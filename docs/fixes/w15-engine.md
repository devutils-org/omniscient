# Fifteenth wave, stream engine — handoff

Branch `feat/w15-engine`. The record of what changed and why is
`docs/families/join.md`, "The fifteenth wave"; this is what the coordinator
needs to merge it.

## Landed, each with its test

1. **`join::decompose_within`** (R's fourteenth-wave request): a paired part
   pairs over the windows `apply_within` was given; `decompose_with` calls it
   on `windows_of(results)`. Test:
   `join::tests::a_paired_part_pairs_over_the_windows_the_joins_were_given`
   (the join's two pairs through `decompose_within`, one through
   `decompose_with`, which is the defect). The two call sites are the
   resolver stream's: filed with the exact calls (below).
2. **A non-negative `AttrValue::Int` read back as `Uint`**: fixed by a
   canonical integer (`AttrValue::int`, `Attr::int` produce `Uint` for
   `n >= 0`) and an equality that holds `Int(n) == Uint(n)`. The wire is
   unchanged. Tests: `event::tests::an_event_survives_a_round_trip_through_json_and_yaml`
   (every `KeyValue` and `AttrValue` variant, `ts_err_ns` 1250, zero,
   negative, `i64::MIN`/`MAX`, `u64::MAX`, compared by `Debug` as well as by
   `==`) and `event::tests::a_non_negative_signed_integer_reads_back_as_what_was_written`.
3. **No row without its evidence level**: `Breakdown` deserializes through a
   check that refuses a row with none, or a whole read from a source with
   none (only `Kind::is_bracket` wholes, the window and the life, go
   without). Tests: `join::tests::a_row_without_an_evidence_level_is_never_built_and_never_read`,
   and `vertical::the_block_io_rows_carry_the_identity_the_block_requests_read_back`
   (the `block-io` rows and whole `IDENTITY`, the device row's readings
   equal to the server's own `done_reads`).
4. **Phase 5**:
   - (a) `join_engine::a_join_under_load_ends_every_state_it_opened_and_holds_no_more_than_its_map`
     (1.5 M events, about 9 s in debug, in a child process with a counting
     allocator) and `join_engine::an_hour_of_joins_leaks_no_state`
     (`#[ignore]`, `OMN_JOIN_LOAD_SECS`). Every count held to the
     generator's; the balance; peak open equal to the capacity; peak heap
     linear (2.0x for twice the stream); nothing live afterwards. The engine
     fixes the test found: the open states' age index (expiry and eviction no
     longer scan the map per event) and unmatched attributions dropped when
     they end (all matches but `wakeup`): 179 MiB of peak down to 97 MiB at
     one million events.
   - (b) `join_engine::a_join_that_evicts_says_how_many_in_results_json_and_in_the_text`.
   - (c) `join_engine::a_new_boundary_is_a_rule_in_a_session_and_no_line_of_engine`.
5. **Filed against my files**: `cross_memory.rs:147`'s skip puts the reason on
   the skip line and indents the rest (`cross_memory::a_skip_puts_the_machine_s_reason_on_the_skip_line`);
   the same layout for `cross_compute`'s and `omn-mcp`'s skips
   (`mcp/tests/common/mod.rs::on_one_line`); `cross_machine.rs`'s churn says
   what the program printed and how it exited. `cross_machine.rs:807` and the
   three `mcp/tests/protocol.rs` tests were already answered by `8e4d7d2`
   (the count asked alone; `run_example_or_skip`).

## Review, fixed

1. `TryFrom<UncheckedBreakdown> for Breakdown` (`src/join.rs`): both
   refusals carried a run of 18 spaces where a line continuation had lost its
   backslash. Fixed; `join::tests::a_row_without_an_evidence_level_is_never_built_and_never_read`
   now holds each refusal to its whole sentence and to no run of spaces.
2. `join_engine::a_program_s_life_is_a_whole_its_three_states_account_for`
   skipped on every machine: it matched `perf_event_paranoid` on stderr before
   it read the exit status, and every successful run prints that caveat. It
   now skips only on a failed run the machine cannot serve, and panics on any
   other failure. On this machine, with the capabilities on, it runs and passes
   (the residue inside its resolution) -- its ground truth is checked for the
   first time since `663be0ad`.
3. The skip-line layout of item 5, in `tests/join_engine.rs` itself: a
   `machine_s_reason` puts the lines that say why the machine cannot serve
   first, on the skip line, and indents the rest under them. Every skip that
   quoted a stderr uses it (the procfs, wakeup, python-session, offcpu,
   overlap-half, per-cpu-refusal and both `/sys` power skips).

## Open, with the measured reason

- **The hour itself**: run here for 300 s in debug (rounds of two million
  events, 24 of them, about 12 s each, every round's peak 194.2 MiB beyond a 427 MiB
  stream, nothing left live), not for 3600 s: five streams and a matrix sweep
  share the machine. By hand, on a quiet one:
  `cargo test --release --test join_engine -- --ignored an_hour_of_joins`.
- **The engine is applied after the run, not streamed**, so its memory is
  linear in the run (the events, and the attributions that matched); what is
  bounded is the open-state map, at the rule's capacity. A streamed join is a
  change to the run model, not scheduled.
- **Phase 5, "adding a boundary means adding a rule"** stays partial: a rule
  cannot choose the kernel or query tier.

## Checks

One `make check`: red on `family_mem::the_working_set_is_what_was_touched_and_not_what_was_held`
(a busiest window of 232 MB against a resident 278 MB, at load 23 to 31),
which passed alone. Then one `cargo test --workspace --no-fail-fast`: 2071
passed, 9 failed, 9 ignored, every failure outside this stream's files. Each
rerun alone once: eight passed (`collect::tests::the_runner_closes_the_window`,
`family_sched::a_task_moved_between_two_cpus_is_counted_once_per_move`,
`scenario_io::a_known_number_of_reads_and_writes_is_counted_call_for_call_in_both_layers`,
`scenario_mem`'s three -- `both_huge_page_backends_read_the_same_promotion_counter`,
`a_gibibyte_of_reads_costs_one_l1_miss_per_cache_line`,
`a_walk_that_fits_in_l1_is_never_timed_and_one_that_does_not_waits_on_memory`
--, `scenario_proc::defect_1_the_first_reading_of_a_launched_target_is_of_the_program`,
`scenario_sched::a_targets_blocking_stack_is_named_while_another_process_floods_the_stack_map`).
`unwind::a_stripped_build_is_named_from_debuginfod_only_when_asked_and_as_the_unstripped_one_is`
failed in the suite and once alone ("the stripped build, named from
debuginfod, does not read as the unstripped one"), then passed twice alone:
for the flaky tally, the unwind stream's.

After the review fixes, one `make check`: red on
`budget::the_extra_cpu_a_faster_poll_costs_is_the_extra_cpu_the_shell_observes`
(in `docs/flaky.md`; shell 62 ms against tool 218 ms at load 14), which passed
alone once. Then one `cargo test --workspace --no-fail-fast`: 2078 passed, 2
failed, 9 ignored; both failures outside this stream's files, each passed alone
once (`scenario_mem::both_huge_page_backends_read_the_same_promotion_counter`,
already in the tally, and `scenario_net::every_datagram_sent_to_a_closed_port_is_a_drop_with_one_reason`,
for the net stream's tally). Clippy with `-D warnings` and `cargo fmt --check`
clean.

## Needs privilege

None. Every test here runs on the capabilities `make` grants
(`sudo -n setcap cap_bpf,cap_perfmon+ep target/debug/omn`, which the
sudoers line in `CLAUDE.md` already allows for the worktrees); the two
end-to-end join tests need none, only `/sys/devices/system/cpu/cpu*/cpufreq`
and `cpuidle`, and skip with the machine's reason without them.

## For docs/DECISIONS.md

- **An integer attribute has one canonical form**: `Uint` when non-negative,
  `Int` only when negative, and the two are equal where they spell the same
  integer. Chosen over a tagged wire format because every consumer of the
  output (`results.json`, ndjson, `omn-mcp`) reads a number as a number, and
  a tag would change every one of them to say what the value already says.
- **A decomposition's whole that is a bracket has no evidence level**: the
  window and the program's life contain the key's events and are joined to
  nothing. Every other row and whole must carry one, and a document without
  it is refused when read rather than shown with a dash.
- **The join engine keeps no record of a state that matched nothing**, but for
  `wakeup`, whose chains count every wakeup.

## Filed for other streams

`docs/open-findings.md`, "Needs from another stream — the fifteenth wave,
stream engine":

- to the resolver stream: swap `src/main.rs` (`assemble`, around line 1234)
  and `src/triage/engine.rs` (around line 220) to `join::decompose_within`,
  with the exact calls, and remove the two `check_paired_parts` refusals;
- to the coordinator: the `docs/ROADMAP.md` exit criteria this stream's tests now
  hold (phase 1 round trip, phase 4 evidence, phase 5 load and evictions to
  `[x]`; phase 5 boundary stays `[~]`).

## Session index rows

None: no session file was added under `examples/`. The two sessions of the
end-to-end tests are written by `tests/join_engine.rs` into
`CARGO_TARGET_TMPDIR`.
