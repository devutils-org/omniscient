# The twenty-first wave, stream run

Branch `feat/w21-run`, from `main` at `156f1c5`. `docs/dev/BRIEF.md`, "run -- the
run's outcome, and what `omn top` opens on": the failure channel, `assemble`,
`top_live`, `--program-stderr` and the manual, the `docs/CATALOG.md` cost-cell
test, and the adopted-orphans denominator proposed with numbers.

## What landed

Five commits, each with its tests.

- **`9f65296` -- a collector that broke the program it measured fails the
  run** (`src/collect/mod.rs`, `src/main.rs`, three lines a collector in
  `src/collect/gpu/{counters,profile,kernel}.rs`, one line in
  `src/emit.rs`'s test literal).
  - `Collector::failure(&self) -> Option<String>`, `None` by default, read
    after `stop`; carried on `Collection::failure` (serialized only when
    set) through an attached slot, a detached one (`Detached::failure`) and a
    recording's documents.
  - `measure` and `record` collect the sentences (`failures`, each once
    however many collections carry it) before `assemble` consumes the
    collections, write every result and artifact as before, then
    `outcome()` prints `error: <sentence>` per failure and exits **1**.
    The results stand beside it: what was read before the break was read.
  - The GPU's inside readers answer it from `died_in_exit`'s own condition
    (`Common::failure` for `gpu.counters` and `gpu.profile`, the same test on
    `KernelCollector`'s senders): a process that said hello, started its exit
    handler (`OP_EXITING`) and never sent `OP_END`. The lost read and the
    `degraded:` line are unchanged: a loss stays a loss.
- **`43ebf0a` -- `assemble` clones only what a join reads; `omn top` opens
  `process` on a target** (`src/main.rs`, `src/collect/mod.rs`).
  - `assemble`: the eighteenth wave's diff as `docs/fixes/w20-shared.md`
    gave it. `join::sources_read(&session.joins, &session.decompose)`
    decides whether a collection's events are cloned into the join stream
    (the bounds still advance, so stream indices hold), and only
    `Tier::Agent` rules are applied in the run; a query-tier rule is the
    stored run's (`omn join`). The `Command::Join` arm is kept.
  - `top_live`: with a launched program or `--pid`, no `--view` and no
    session file, the first screen is `process`; without a target it stays
    `system`.
  - The live results' `value_attr`, `row_stamp` and `drains`: a new
    `Escalate::described(&mut self, &[Described])` (default no-op) is shown,
    before each round's `decide`, what each slot's collector says now --
    value attribute, row stamp, the drops placed at their drains. The live
    `Tap` keeps the opening `Data` as a shell and, **while no event has been
    handed over**, sends it again as `Update::Restart` with those fields set,
    once per change. After the first events it stops: a restart then would
    wipe what the view holds, and the feed has no update that sets them on a
    result already drawing (filed to views below). The end of the run carries
    all three as before.
- **`4513c43` -- `--program-stderr`, and the stop's subreaper in the
  manual** (`src/cli.rs`, `src/main.rs`, `man/omn.1`, `tests/runner.rs`).
  - `--program-stderr FILE`, global, sets `OMN_PROGRAM_STDERR` in `run()`
    before anything is started (the launch reads the variable and does not
    pass it on), so the flag wins over the variable. Beside a command that
    launches nothing (`--pid`, `list`, `diff`, a replay, ...) it is refused
    with exit 2 and the reason (`Cli::program_stderr_refusal`); a `-f`
    session is allowed, since it may launch.
  - `man/omn.1`: the flag under Target; `OMN_PROGRAM_STDERR` naming the flag;
    a stop paragraph under `-- program` (the tree stopped then killed,
    `lifetime.terminated`; `omn` the subreaper, orphans ended at the end of
    every run, `lifetime.adopted_ended`; `lifetime.left_running` for what
    could not be signalled); exit 1 for a collector that broke its program;
    `process` the first screen of `--pid` too.
- **`ca45f35` -- `docs/CATALOG.md`'s Cost column held to the catalog's chain**
  (`tests/cli.rs`, docs' request).
  `the_catalog_document_s_cost_column_is_the_catalog_s_chain` reads every
  family table with a `Metric` and a `Cost` column, expands a grouped row
  (`.x` as the family's `x`, or beside the first id's stem: `mem.cache_l1` /
  `.llc` is `mem.cache_llc`), and holds the cell to the tiers each chain
  declares in order without repeats -- each member's alike, or the members'
  chains end to end. A row naming a source no catalog file holds fails. 87
  rows read today, all agreeing.
- **`1c15958` -- a launched program's denominator takes in its adopted
  orphans** (`src/collect/{mod,task,launch}.rs`, `tests/budget.rs`).
  When `omn` adopts (`launch::adopting()`), `Meter::Launched` reads
  `getrusage(RUSAGE_CHILDREN)` plus every live descendant of `omn`
  (`task::descendants_cpu`): the program's tree and the orphans that left it,
  in one walk. A library caller, which adopts nothing, keeps
  `task::tree_cpu`. The cost record's `observed_from` says "its orphans
  adopted by omn included".

## Shown versus reasoned

- **Shown**: `failures`/`outcome` by
  `a_collector_that_broke_its_program_fails_the_run_once` (deduplicated, exit
  1 with one, 0 without, `failure` absent from a collection's JSON unless
  set). `Tap::describe` by
  `a_live_view_is_told_how_to_read_its_results_before_its_first_events`
  (`drain_close` spelled as the view reads it, unchanged is silent, nothing
  after the first events). `--program-stderr` by
  `the_program_s_stderr_goes_to_the_file_the_flag_names` (the file holds the
  program's line, `omn`'s stderr does not, the variable's file is not
  created) and `the_program_s_stderr_flag_without_a_program_is_refused`.
  The cost-cell test failing on a mutated `docs/CATALOG.md` (`cpu.cycles` set to
  `med`, the `mem.cache_*` row to `free`): both lines named, with each
  member's chain. The man page rendered with `groff -man`.
- **Shown, the denominator, with numbers** (debug build, this shared
  machine): a program that sleeps 2 s while a `setsid` orphan burns 1 s of
  cpu, under `mem.rss -m poll@100ms -e summary --budget 5%`:
  - before (`main`'s meter): **3 runs of 3 moved off `fs:proc:smaps_rollup`
    onto `fs:proc:statm`** mid-run, the mid-run denominator being the
    sleeping shell's few milliseconds; the final figure was 1.01 s either
    way, because the orphan is reaped into `RUSAGE_CHILDREN` at the end.
  - after: **3 of 3 stayed on `smaps_rollup`**, at 0.73 to 1.10 % (and at
    2 % budget, 0.94 to 1.10 %, held, where the old binary moved).
  - what the wider read costs: with the orphan, the tool's own cpu over the
    run went from a median of 5.9 ms (8 runs, 4.9 to 6.2) to 8.05 ms (8
    runs, 7.2 to 8.8), interleaved: one more process read and one
    `getrusage` per meter read. Without an orphan (`sleep 2`), 4.7 against
    4.6 ms median over 6 runs each: no difference.
  - `an_orphan_the_program_left_is_weighed_with_it_while_it_runs` holds the
    "after" side (observed cpu >= 0.5 s and `served_by` the one backend). It
    was not run against the old binary (`BINARY` is this build's); the
    "before" side is the three manual runs above.
- **Reasoned**: the GPU's `failure()` on a real CUDA program killed in the
  library's exit handler (no GPU run in this stream; the condition is the
  one `unfinished`/`stop` already push `died_in_exit` on). `omn top` opening
  `process` with a terminal (there is none here; the change is the
  `start_on` default). The restart reaching a drawing view: the view's
  `feed::apply` replaces its data with a `Restart`, and nothing was handed
  over before it.
- **`assemble`**: the query-tier filter and the clone guard were checked by
  `make check` (below); no new test, as `w20-shared.md` says
  `join_engine`'s query-tier test holds either way.

## The suite

One `make check` at `1c15958`, after `make` (capabilities granted): 1662
passed, 9 ignored, **1 failed** --
`tests/budget.rs::a_request_attached_for_part_of_the_run_is_weighed_over_its_own_window`,
`assert!(ns(&whole, "wall_ns") >= 750_000_000)` with the whole request's
window at **749 953 597 ns**, 46 us short of its 750 ms duration (its program
spent 744.8 ms; the part's window 250.9 ms). Rerun alone once: passed. Not
the meter's change -- the assertion is on wall time, which this stream did
not touch -- and not in the flaky tally yet: a window that closes a hair
before the duration measured from an earlier origin. Filed for the tally
below. `make check` stopped at that failure, so clippy and fmt ran on their
own: `cargo clippy --workspace --all-targets -- -D warnings` and
`cargo fmt --check`, both clean on this tree.

## Not done, and why

- **Live drains after the first events, and a source attached late**: their
  fields reach the view only at the end, because `view/src/feed.rs` has no
  update for them (filed to views). The tap's restart is the bridge until
  then.
- **`omn latency --runs N` with `--program-stderr`**: each launch truncates
  the file, so it holds the last run's stderr. Said nowhere yet; the triage
  driver is not this stream's.
- **The failure channel in triage** (`src/triage/engine.rs` runs
  `run_all_within` and reads no `failure`): no playbook asks a GPU inside
  reader today, so nothing reaches it yet. Filed to triage's owner below.
- **A helper's orphan**: a process a helper leaves behind when it exits is
  reparented to `omn` and is then weighed (and ended) as the program's.
  Neither `curl` nor a container runtime's `inspect` leaves one.

## Privileged runs wanted

On the worktree's binary after `make`, with an NVIDIA GPU, the injection
library built:

```console
cd /home/moha/repos/omniscient-w21-run && make
cargo test --test gpu_counters -- --nocapture
# A program killed in the library's exit handler, if one reproduces it:
target/debug/omn gpu.counters -- <cuda program>; echo "exit $?"
```

expected: the `degraded:` line as before, then `error: pid N died inside
omn's injection library ...` and exit 1.

## Decision lines

- **A collector that broke the program it measured fails the run, after the
  results are written**: exit 1 with the sentence on stderr; the results,
  their lost read and their `degraded:` line stand. If the run exited 0, a
  script reads the tool's failure as the program's clean run.
- **An adopted orphan's cpu is in the launched program's denominator while
  it lives**, read as `omn`'s live descendants plus its children account when
  `omn` adopts. Proposed with the numbers above: without it a 5 % budget moved
  a 1 % request onto a worse backend in 3 runs of 3; with it, 0 of 3, for
  about 2 ms of the tool's cpu over a 2 s run when an orphan exists and none
  measurable when none does. `docs/DECISIONS.md`'s row ("the meter's change is to
  come") can drop that clause.
- **`--program-stderr` is refused beside a command that launches nothing**,
  as any flag that cannot mean anything is.

## Needs from another stream

### To views (`view/src/feed.rs`): an update describing a result

An `Update::Described { index, value_attr: Option<String>, row_stamp:
Option<String>, drains: Vec<Drain> }`, applied by setting the three fields on
`data.run.results[index]`. `Tap::described` (`src/main.rs`) would then send it
on every change, not only before the first events, and the restart bridge
goes.

### To data (`src/emit.rs`): one line in a test literal

`two_threads` gains `failure: None,` (`Collection::failure`). Data's
`Collector::fields()` will touch the same literal and the same tuples in
`src/collect/mod.rs` (`run_core`, `Detached`, the recording's documents):
a mechanical conflict, both sides kept.

### To the coordinator (`docs/DECISIONS.md`)

The adopted-orphan row: the meter change is done (`1c15958`); and the
decision line on the failure channel above.

### To the coordinator (`docs/open-findings.md`, the flaky tally)

`a_request_attached_for_part_of_the_run_is_weighed_over_its_own_window`:
once, the whole request's window 46 us short of 750 ms; passed alone. If it
recurs, the floor can be the duration less a poll (`>= 730_000_000`), since
the window opens after the attach and the deadline counts from before it.

### To triage's owner (`src/triage/**`): the failure channel

`engine.rs` could read `failure` from its collections as `main.rs`'s
`failures` does, and fail the triage the same way, once a playbook asks a
source whose collector can break its program.

### To views or process (`tests/view_live.rs`): the gallery and `process`

`a_gallery_prints_every_view` fails on `main` at `2ba6626`, before this
round's commits (checked with them stashed): the gallery at `100x20` does
not show `process`. Not this stream's. Beside it, in the same run,
`tests/view_workloads.rs::each_session_asks_its_views_sources_or_says_why_not`
fails reading files alone (the ledger view needs `sched.offcpu`, which its
workload's session does not ask and its manifest does not excuse): this
round touches no view, session or manifest.

### To triage's owner (`src/triage/**`): `--program-stderr` and `--runs`

Whether `omn latency --runs N` should write `FILE.<n>` per run, or append.

## Review

Two findings, both fixed.

- **`top_live` read no failure** (`ce5cde2`, `src/main.rs`). Right: the
  process view draws `gpu.*`, and `omn top -- <cuda program>` whose program
  the injection library killed exited 0 with only the `degraded:` line.
  `top_live` now takes `failures(&collections)` before `assemble` consumes
  them and ends with `outcome(&failures)`, after the screen is given back,
  the costs are said and `--out` is written: `error: <sentence>` per
  failure, exit 1. **Reasoned**, not shown end to end: no collector can be
  made to break its program here without a GPU; `outcome` is the function
  `a_collector_that_broke_its_program_fails_the_run_once` holds, and
  `tests/view_live.rs`'s headless runs still exit 0 (4 of 5 pass; the fifth
  is the gallery's, failing on `main` before this round, above).
- **The meter counted `omn`'s own helpers as the program's** (`c1ba7ba`,
  `src/collect/{launch,task,mod}.rs`, `src/unwind.rs`, `src/request.rs`).
  Right, and it fails in the tool's favour. The handoff named `nvidia-smi`
  too; it is wrong there: `nvidia-smi` is run only by tests. The helpers
  `omn` runs outside its tests are `curl` (debuginfod, up to 60 s) and a
  container runtime's `inspect`. Both now go through
  `launch::helper_output`, which runs a command as `Command::output` does
  and:
  - lists the helper while it runs (`helpers_running`), so the meter's walk
    (`task::descendants_cpu(me, except)`) leaves it and its children out,
    and so does `end_adopted`, which would otherwise stop and kill a
    `curl` still running when a run ends, as an orphan;
  - once it has exited, reads its whole usage with `waitid(WEXITED |
    WNOWAIT)` -- the zombie's `rusage`, the amount the reap is about to add
    to the children account -- adds it to `helpers_spent_ns`, then reaps.
    The meter reads the children account first and the set-aside second,
    so a helper reaped between the two reads weighs the program short by
    it, never long (and the high water keeps the earlier, larger reading).
  - The non-adopting meter's fallback, the children account once the
    program is reaped, takes the set-aside out too.
  **Shown**: `a_helper_s_cpu_is_set_aside_from_the_children_account` (a
  helper burning 200 ms: the set-aside grows by at least that, the children
  account by at least the set-aside, and stdout and stderr come back as
  `output()` gives them) and
  `a_process_left_out_of_the_descendants_cpu_takes_its_own_children_with_it`
  (a shell left out takes the python child burning 300 ms under it: 0 ns).
  Neither skipped here. **Not shown**: a run with a live `curl` mid-run;
  no test makes `omn` fetch from a debuginfod server.

The suite for this round, after `make` (capabilities granted):
`cargo test --workspace --no-fail-fast` at `c1ba7ba`, 2674 passed, 13
ignored, 3 failed -- the gallery's and the workloads' tests above, both on
`main` already, and `join_engine`'s
`another_namespaces_lo_is_not_credited_to_this_ones`, in the tally, which
passed rerun alone once. `cargo clippy --workspace --all-targets -- -D
warnings` and `cargo fmt --check` clean.
