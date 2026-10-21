# w26 main — the commands in the library

Phase 0a of the twenty-sixth wave (`BRIEF.md`, "0a · `main`").

## Landed

- `src/main.rs` is 107 lines: parsing, the implicit `trace`, dispatch to
  `omn::command::run`, and the exit codes of a failure. It was 4 435.
- `src/command/`: `exit` (codes, `Whose` to a code, what a run says of which
  one it is), `trace`, `budget` (the ladder, the ceiling's note and refusal),
  `cost` (what a run measured itself spending, the backend comparison), `run`
  (record, assemble, measure, write), `list` (`list`, `formats`, `validate`,
  `selftest`), `triage`, `top` (the live view's tap and its cpu account).
  Moved, not rewritten: every function is the one it was, `omn::` read as
  `crate::`, made `pub(crate)` where a sibling reads it.
- `src/repeat.rs` was compiled twice, once into the library's file list by
  nobody and once into the binary by `mod repeat;`: it is a library module
  now, and the binary's copy is gone.
- Gone as duplicates or pass-throughs: `thread_cpu_ns` and
  `process_cpu_ns` (the runner's, `collect::thread_cpu_ns` /
  `collect::process_cpu_ns`, now `pub(crate)`), `weighs_its_samples`
  (`collect::weighs_its_samples` directly), the `mod cli` shim.

## Shown

- `cargo test --lib command`: the 17 tests that were the binary's (the
  budget ladder's among them: `a_sampled_rung_whose_events_are_not_weighed_is_passed_over_not_taken`,
  `a_band_per_regime_says_which_programs_fit_the_ceiling`) run as library
  tests; `cargo test --bin omn` keeps the two `normalize` tests.
- `omn list --defaults --format json` byte-identical; `--test cli` (33),
  `--test budget` (27), `--test runner` (32) green.

## Not done

- The modules are the old functions regrouped; none is deeper for it. The
  interface that matters, one run entry point, is `runner`'s (0b).
