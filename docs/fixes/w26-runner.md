# w26 runner — one run entry point, and the runner in files by concern

Phase 0b of the twenty-sixth wave (`BRIEF.md`, "0b · `runner`").

## Landed

- **`collect::Run`**, the one entry point: `Run::new(&resolved)` with
  `cheaper`, `unserved`, `bound`, `escalation` and `recorder` as options,
  `go()` returning `Ran { collections, recorded }`. It replaces `run`,
  `run_all`, `run_all_within`, `run_all_within_unserved`, `run_all_bounded`,
  `record_within`, `record_bounded` and `run_all_escalating` -- eight names
  for one `run_core`, whose differences are now four options and one
  default each (no ladder: stop when over budget; no escalation given: the
  session's own; a recording: none).
- **`collect/mod.rs` split by concern**: `outcome` (collections, losses,
  watched windows), `overhead` (the tool's cpu and what collectors spend
  inside the target), `ask` (what the runner asks of its caller: a cheaper
  rung, an escalation, a recorder), `run` (`Run` and the core), `spending`
  (the budget held while the run goes), `drive` (the rounds, the ledger),
  `recording`. Moved, not rewritten; items made `pub(super)` where a sibling
  reads them. `mod.rs` keeps the traits, the registry and its queries: 646
  lines of code, beside its 2 850 lines of tests.
- **`join/decompose.rs`**: the decomposition engine out of `join.rs`
  (1 398 lines), reading the join's attributions through its parent.

## Shown

- `cargo test --lib collect` (837) and `--lib join` (99) green;
  `--test runner` (32), `recorder` (8), `budget` (27), `triage` (30),
  `cli` (33), `join_engine` (26), `cross_compute` green.
- `examples/live.rs` and the two documents that named the old functions
  (`docs/recorder.md`, `docs/triage.md`) follow.

## Counts

| | before | after |
|---|---|---|
| public run entry points | 8 | 1 |
| largest file under `collect/`, lines of code | 5 435 (`mod.rs`) | 1 371 (`launch.rs`) |
| `join.rs` | 7 137 | 5 747, and `join/decompose.rs` 1 398 |
