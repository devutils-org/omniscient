# The twenty-second wave, stream reference

The shared runner every family's pairs go through, its schema, the index of
the verdict tables, and the CI's skip lines for the reference tools. Landed
in `9c502a5`, compiling and clippy-clean; its own tests were **not run**
in this stream (below).

## What landed

- **The runner** (`tests/reference/runner.rs`). It reads
  `catalog/references/<family>.yaml` (`serde_norway`; an empty file or `[]`
  is no pairs), checks each pair as data, runs `omn` and the tool, parses
  the tool's output with the family's parser, and compares each value with
  the median over `runs`. Two run modes:
  - `separate` (default): `omn <omn> -- <workload>`, then
    `<command> <workload>`, alternating which goes first.
  - `attach`: one run both observers see. The workload is started through
    `sh -c 'kill -STOP $$; exec "$@"'`, so its pid exists before it execs.
    Both observers start with `{pid}` substituted. After `settle_ms` it gets
    `SIGCONT`. A first version stopped the child in `pre_exec`, which
    deadlocks, because `std` waits for the child's exec. Rewritten before the
    commit.
  - Placeholders: `{tmp}` (a scratch directory per run), `{out}` (`omn -o`'s
    directory, whose `results.json` is then read instead of stdout), and
    `{pid}`. A `report:` command (`perf report` after `perf record`) replaces
    the tool's output.
  - The `omn` path: dotted fields, indices, `*`, and `[field=value]`
    filters, with matches summed. NDJSON is read as an array.
  - Tolerance: `{rel, abs}`, and the larger of the two is allowed. There are
    `omn_scale` and `reference_scale` for unit changes. `key: "*"` compares
    every key the tool gave, with `{key}` in the path.
  - Outcomes: pass (one `same numbers:` line per value), skip
    (`skipped: reference <family> <label>: <reason>`, with the refusing
    side's lines indented under it), gap (a pair's `gap:` field, not run),
    and fail (every failing pair of the family named in one panic).
  - `omn`'s exit status is read as the manual words it. 1 skips, quoting its
    stderr. 2 fails. 3 fails, asking for `gap:`.
- **`tests/reference.rs`**:
  - one test per family, `pairs_<family>`, so a stream runs its own pairs
    alone, and one pair alone with `OMN_REFERENCE_PAIR=<label substring>`;
  - `families_are_whole`: the YAML files, the modules and the
    `docs/replaces/*.md` tables name the same eleven families, and
    `docs/replaces.md` links each;
  - `every_pair_is_well_formed`: runs nothing. A parser name that exists, a
    tolerance, a `why`, `{pid}` only under `attach`, workload files that
    exist, unique ids, unique parser names, and a path that parses;
  - `every_row_names_a_pair_or_why_none`: each table row holds a pair's
    `` `id` ``, or a pair's `source` in the row with its `tool` in the first
    cell, or `no pair` and why;
  - unit tests for the path syntax, NDJSON, tolerance pass and fail, `*` keys,
    attach mode (both observers get the workload's pid), and each skip and
    fail cause. These run `sh` in place of `omn`.
- **`tests/reference/common.rs`**: `root()`, `capture(family, name)` for the
  parser tests, `number()` (thousands separators, `%`, and `<not counted>`
  as an error, never zero), `value()` and `installed()`. The `parsers()`
  signature is unchanged.
- **`catalog/references/README.md`**: the schema, run modes, placeholders,
  path syntax, skip rules and table rules.
- **`docs/replaces.md`**: the index. Its "Across families" summary is empty
  until the families merge (below).
- **`.github/expected-skips.txt`**: three lines for the reference skips: a
  binary not installed, `sudo -n` not granting root, and a tool refused by
  the machine. When `omn` cannot run a pair here, the skip quotes `omn`'s
  stderr, and the existing lines already match that.

## Shown vs reasoned

- **Shown:** `cargo clippy --workspace --all-targets -- -D warnings` is
  clean. `cargo fmt --check` is clean. `make` built `omn` and granted the
  capabilities. `omn cpu.instructions -m count -e summary --format json`
  printed the shape the README's example path reads
  (`results[source=...].outputs[emit=summary].series.*.mean`).
- **Reasoned, not run:** every test in `tests/reference.rs`, including the
  runner's unit tests. The first `cargo test --test reference` hung on the
  `pre_exec` stop described above. I killed the hung processes and fixed the
  cause, but the rerun was denied by this session's permission policy, so
  none of these tests has passed yet. Coordinator: run them first (below).
- The existing family files are skeletons with empty YAML and tables, so on
  `main` the pair tests pass trivially and the row test checks nothing.

## Not done, and why

- **The cross-family summary in `docs/replaces.md`** ("Replaced fully / in
  part / not replaced") needs the families' tables. Fill it at the wave's
  merge from `docs/replaces/*.md` and the `pairs_*` output.
- **`.github/expected-skips-stock.txt`**: not changed. The reference skips
  are the same on both cells.
- No `privileged` pair exists yet, so the `sudo -n` path has run only in the
  unit test for its skip.

## Privileged runs wanted

None of the runner's own. A family's privileged pairs need
`sudo -n` to grant the tool. That is in each family's handoff.

## Decision lines

- A pair's `omn` exit 1 is a skip, quoting `omn`'s reason; exits 2 and 3
  are failures. Exit 1 is a failure instead when `omn` wrote its results
  first (something failed while running), when the workload fails with no
  observer, or when an attached workload exited non-zero on its own. A use `omn` cannot serve is declared with `gap:`, not left
  to fail.
- A tool's non-zero exit is a skip only when its stderr carries a refusal
  wording (built-in list, plus the pair's `refused_when`) and the workload
  succeeds when run alone: under `run: separate` the workload's stderr is
  part of the tool's. Any other parse failure fails the pair.
- Several matches of an `omn` path are summed, so per-thread series add up
  to the process.
- The tolerance allowed is the larger of `rel` and `abs`.

## Needs from another stream

- **Every family stream**:
  - Write the tables so each row is backed (README, "The tables"). The
    simplest way is to put the tool's name, as the pair's `tool` field
    spells it, in the first cell, and the source anywhere in the row.
  - Use `-o {out}` in `omn` whenever the workload prints anything.
  - Declare a use `omn` does not serve with `gap:` rather than a failing
    pair.
- **Coordinator, before merging any family**:
  - Run `make && cargo test --test reference -- --nocapture` in this
    worktree. The runner's unit tests (`runner::tests::*`, `common::tests::*`)
    have to pass first.
  - If `both_observers_see_the_same_attached_pid` hangs, the cause is the
    attach start-up (`run_attached`). Look at a stopped `sh` in `ps`.

## Review

Fix round after the merge into main.

- **`tests/scenario_proc.rs`, `tests/scenario_cpu.rs`: copying `catalog/`
  breaks on `catalog/references/`.** The reviewer is right. The same
  `is_file()` guard as in `tests/cli.rs` and `tests/triage.rs` now skips the
  subdirectory in both loops. No other test reads `catalog/` entry by entry.
  `a_machine_that_cannot_serve_a_valid_request_has_its_own_exit_code` and
  `a_backend_that_cannot_serve_the_mode_is_passed_over_before_the_launch`
  pass.
- **`judge_omn`: every exit 1 was a skip.** Fixed in the runner, without
  changing `omn`'s exit codes. Exit 1 now fails the pair in three cases. The
  first is when `omn` wrote its results first (`-o {out}`), which is a run
  that reached its end and then failed. The second is when an attached
  workload exited non-zero on its own (`run_attached` keeps its status). The
  third is when the workload fails with no observer. Only what is left is a
  skip. One case is still read as the machine's: `omn` failing mid-run
  before it writes anything, against a healthy workload. Telling that apart
  needs `omn` to say which half of exit 1 it is giving, which is a change to
  `src/main.rs` and not to this stream's runner.
- **`judge_tool`: a workload's "Permission denied" read as a refused
  tool.** Fixed. Before a refusal is granted as a skip, the workload runs
  once alone (`workload_fails_alone`). If it fails there, the pair fails
  and the failure quotes the workload's stderr. This runs only on the way
  to a skip, so a pair that runs pays nothing for it. The same check comes
  before an `omn` exit 1 is skipped.
- Covered by `runner::tests::a_failure_that_is_not_the_machines_is_not_a_skip`
  (four cases).
- **Found on the way, not this round's:** on merged main, `cargo test
  --test reference` has 11 failures. The same 11 fail with this round's
  changes stashed. They come from the families' pair files, which use forms
  the runner's validation refuses: `{pid}` outside `run: attach` (top, pmap,
  perf stat) and `[0]` in compare paths (perfrecord). They also come from
  rows that are not backed. Each family's pairs need fixing, or the path
  grammar needs `[N]`. That is the coordinator's call.
