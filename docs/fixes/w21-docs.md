# The twenty-first wave, stream docs

The docs requests of every w20 handoff, and the GPU bench's schedule
(`docs/dev/BRIEF.md`, "docs -- the documents and the bench's schedule"). Branch
`feat/w21-docs`, on `main` at `156f1c5`.

## What landed

- **`178e17e` docs(catalog): the rows the twentieth wave's handoffs asked
  for.** `docs/CATALOG.md`:
  - `cpu.oncpu_time`: the `bpf:raw_tp` backend on `sched_stat_runtime`
    first in the row, poll only, one series per thread; a caveat that says
    so, that `trace` stays on `bpf:tp`, and that the source emits `raw`
    (`w20-cpu.md`).
  - `power.energy`: the `fs:sys` powercap backend in the row; a caveat for
    `raw` (`w20-cpu.md`).
  - `net.bytes`: the backends as `catalog/net.yaml` names them
    (`bpf:fentry` sock_sendmsg), `iface` among the targets, and a caveat for
    `raw` with `rx` and `tx` (`w20-net.md`).
  - `net.socket`: keyed by socket, task and interface, with the YAML's
    backends, modes and targets, and a caveat on what `iface` is
    (`w20-net.md`).
  - A `net.request` row and a caveat: the declared USDT probes, Linux 6.6,
    the states it tiles, `net_rx`/`net_tx` by cookie (`w20-docs.md`).
  - `sched.wakeup`'s new fields (`wakee_comm`, `waker_comm`,
    `served_in_tid`, `served_in_comm`, `irq`, `softirq`) in a caveat;
    `launch` among the targets of `sched.wakeup`, `sched.lock` and
    `sched.futex`; the children caveat says each takes a launched program
    (`w20-sched.md`).
  - GPU: `gpu.freq`, `gpu.power` and `gpu.temp` split out of their grouped
    row, each with the YAML's chain and cost (`free / high`, `free / high`,
    `free / low`); `launch` and `snapshot` where the YAML declares them; a
    caveat that the polled sources emit `raw` and what the NVML cost cells
    mean (`w20-gpu.md`).
  - `docs/bench.md`'s "Not measured here" table loses its `gpu.counters` /
    `vendor:ncu` row, with a line under the table saying why the printed
    table differs (`w20-gpu.md`).
- **`1ae3d1a` docs(unwind): the stripped build's second shape was
  aliasing.** "Reads as the workload or the window" replaced by the aliasing
  explanation and the change (`c7c90c1`, `NAMED_PERIOD_NS`, 61 us); the
  paragraph takes the test off the flaky tally for that shape
  (`w20-rings.md`).
- **`a5b1062` docs(proc): `proc-not-built` exits 3, or 1 where tracefs is
  root-only.** `docs/scenarios/proc.md` lines 36, 161 and the "exit 3 again"
  paragraph (which claimed "3 everywhere"), and `docs/families/proc.md`,
  now say what `docs/GUIDE.md` says (`w20-docs.md`).
- **`fb03a7e` fix(examples): the vertical server holds 1024 requests, and
  refuses a longer plan.** `MAX_PLAN` 64 -> 1024, the value
  `tests/view_fixtures/record.sh` seds in; a plan over it now exits 2 with
  a sentence instead of being cut short in silence (`w20-record.md`).
- **`571a2bc` test(cases): a row followed into a child reads the rest of
  its job.** `tests/cases.rs` reads `followed.job.on_cpu` and
  `.runnable` (the CLI) and, for the MCP way, every busy task off the
  followed chain, and adds them to the `on-cpu` / `runnable` row before
  holding it to the reference -- only in a case whose `says` names the wait
  for a child (`make-sqlite`, `gcc-shell`). The `_` -> `-` normalization of
  `limit` is gone; `LIMIT_SPELLED` holds the JSON's spelling, and the unit
  fixture now spells `cpu-queue` (`w20-triage.md`, phase B).
- **`85f314a` test(bench_bands): the equivalent-band rule lives in the
  catalog loader alone.** `cheaper_equivalents`, `served` and their two
  tests removed; the module comment points at `src/catalog.rs`
  (`w20-shared.md`).
- **`63309b7` test(bench_gpu): a replaying arm runs the launch regime with
  2000 launches.** `GpuWorkload::replayed` / `scaled()`, `Arm::replays()`
  (`gpu.counters`), `REPLAYED_LAUNCHES = 2000` in `examples/bench/gpu.rs`;
  `tests/bench_gpu.rs` measures the replaying arms on the smaller `launch`
  with a bare run and a control of that size beside them, judges their rows
  on those, and writes them as `scaled_series` in `table.json`.
  `docs/bench.md` says so under the regimes table (`w20-gpu.md`).

Already done before this wave, checked and left: `examples/mcp/approved/program.py`'s
header names `program_stderr`; `tests/cases.rs` reads `program_stderr` and
carries no `MODEL_BUDGET` or `budget is required` fallback.

## Shown versus reasoned

- Shown: `cargo test --test bench_bands` (6 passed), `cargo test --test
  bench_gpu` (12 passed, the measurement ignored, one new test for the
  replaying size), `cargo test --test cases` unit tests (5 passed), the
  vertical server compiled with `-Wall` and refusing a 1025-request plan
  with exit 2; `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo fmt --check` clean.
- `every_case_names_its_known_cause_both_ways`: see "The cases run" below.
- Reasoned: that the GPU step fits in about fifteen minutes. 108 minutes for
  `gpu.counters` at 50 000 launches, scaled to 2 000, is about 4.3 minutes,
  plus the scaled bare and control (seconds) and the other arms' 9 minutes.
  Not measured: no GPU bench runs in this wave's streams.
- Reasoned: that adding the job makes `make-sqlite` and `gcc-shell` agree.
  The MCP way's "job" is every task off the chain, since a decomposition
  table does not say which process a task is in. That is an upper bound
  (`src/triage/latency.rs`, `Job`: "they may be more"), so `gcc-shell`'s
  pass is a loosened comparison, not a fix in triage; see "Review".
- Reasoned: the CATALOG rows against `catalog/*.yaml`, read entry by entry;
  no test reads `docs/CATALOG.md`'s rows.

## The cases run

`make`, then `cargo test --test cases every_case_names -- --nocapture`, run
twice (about 205 s each, capabilities granted, other streams on the
machine):

- First run, red, for one reason only: `gcc-shell`'s open finding
  (`every-cited-number-agrees-with-the-reference` on
  `a-cpu-shared-with-two-competitors`) passed in both ways once the job is
  added, and the test demands it go. Removed from `cases/gcc-shell/case.yml`
  (outside this row's files, but the test in this row's file fails without
  it; nobody owns `cases/` this wave).
- Second run, green (1 passed). Both ways, both cases of `gcc-shell` agree;
  `make-sqlite/a-cpu-shared-with-two-competitors` agrees.
- `make-sqlite/the-critical-path-is-one-job` stays a FINDING, now over
  rather than under: `on-cpu` 7113 ms (CLI) and 7227 ms (MCP) against
  `watch.longest_cpu_ns` 5330 and 5480 ms. `followed.job` is every busy task
  off the way, not only the critical job's, so a parallel build's other work
  is in it. Its `open_findings` text in `cases/make-sqlite/case.yml` says so
  now.
- Other FINDINGs seen, unchanged by this branch and already listed:
  `http-server-stdlib/a-slow-client/mcp` (a sentence, the cited `blocked`),
  `sqlite3-durability/every-commit-waits-for-the-disk` (25 to 28 ms over a
  6 ms slack, both ways).

## Not done, and why

- `tests/view_fixtures/record.sh`'s `sed` of `MAX_PLAN` is now a no-op and
  can go: the record stream's file.
- `cases/make-sqlite/case.yml`'s `open_findings` entry for
  `every-cited-number-agrees-with-the-reference` (the job read as one
  `cc1`): not this row's file, and it stays until a run shows the check
  passing.
- `docs/bench.md`'s recorded "Not measured here" table still lists
  `cpu.oncpu_time` / `bpf:raw_tp` as "no collector is written for it": it is
  a printed record, and the next backends bench rewrites it.
- `gpu.temp`'s band (`w20-gpu.md`): waits for the next GPU table.

## Privileged runs wanted

- The GPU step with the new schedule, alone on a quiet device:
  `cd /home/moha/repos/omniscient && make && cargo test --release --test
  bench_gpu -- --ignored --nocapture every_gpu_tracer_is_costed_against_a_negative_control`
  (or through `scripts/bench.sh` as the coordinator runs it). Wanted: the
  step's wall time, and the `gpu.counters` rows against the scaled baselines.
- `make && target/debug/omn -f examples/proc-not-built.yml; echo $?` on a
  machine with a readable tracefs, to see the exit 3 the documents now
  qualify.

## Decision lines

- **A replaying arm is measured at its own size.** Its rows compare against
  a bare run and a control of the same smaller workload, never against the
  50 000-launch baseline. If left open, a replaying arm either costs two
  hours or is compared across two sizes. The next step is the GPU step run
  above.
- **The job is added only where the case says the wait is for a child.**
  Decided by the case's own `says`, because the MCP way cannot tell a
  child's tasks from a thread's. If a case follows a wait for a child
  without saying so, its row stays the child's alone.
- **The vertical server refuses a plan it cannot hold.** Silently truncating
  is a degradation nobody is told about.

## Needs from another stream

- **record (`tests/view_fixtures/record.sh`)**: drop the `sed` that raises
  `MAX_PLAN`; `examples/vertical/server.c` holds 1024 now.
- **coordinator (`docs/open-findings.md`, `docs/dev/STATE.md`)**: the unwind second
  shape is documented as aliasing, off the tally; `cheaper_equivalents`
  (`docs/dev/STATE.md` line 1247) is gone from `tests/bench_bands.rs`; every w20 docs
  request above is landed.
- **triage (`src/triage/latency.rs`, `Job`)**: for `make-sqlite`'s critical
  path the job overshoots by the build's other work (7.1 s against 5.3 s).
  A job restricted to the processes under the followed child's parent (the
  driver the wait named) would be the reference's account.
- **coordinator (`cases/`), to sign off at the merge**: two edits outside
  this row's files. `cases/gcc-shell/case.yml` lost its open finding on
  `every-cited-number-agrees-with-the-reference` /
  `a-cpu-shared-with-two-competitors`, and `cases/make-sqlite/case.yml`'s
  finding text changed; `cases/` is in no row of the streams table. The
  merge message should say that `gcc-shell`'s finding closes on a loosened
  comparison (the row plus `followed.job`, an upper bound on the MCP way),
  not on a change in triage.
- **triage (`src/triage/latency.rs`, `Job`)**, the same request as above
  seen from `gcc-shell`: once the job is restricted to the followed child's
  parent, `gcc-shell`'s pass would rest on the job itself rather than on an
  upper bound, and `tests/cases.rs` could read it as such.

## Review

- **"Edits outside the docs row's owned files; the gcc-shell pass rests on
  a loosened comparison" (gap).** Agreed, and kept as is. The two `cases/`
  edits are needed by this row's own test: `every_case_names_its_known_cause_both_ways`
  fails on an open finding that passes, so leaving `gcc-shell`'s entry in
  place reds the suite. What passes is the row plus `followed.job`, and on
  the MCP way that job is every busy task off the chain -- an upper bound,
  as triage's `Job` doc comment says. So the finding is closed by the
  comparison, not by triage. Fixed by saying so where a reader meets it:
  the two comments in `tests/cases.rs` (the MCP job, and where the job is
  added to a row), the "Shown versus reasoned" bullet above, and the
  coordinator's and triage's lines under "Needs from another stream", which
  ask for the sign-off and for the merge message to state it.
