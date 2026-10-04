# Brief — what the programs found

The eighteenth wave: everything `STATE.md` listed under "After the seventeenth
wave", items 3 to 11, and what the CI read at `b37a925` found
(`docs/open-findings.md`, "The CI at `b37a925`, read back"). No new phase: the
seventeenth wave pointed the tool at programs nobody wrote for it, and this
wave fixes what those programs showed -- the triage answering from the wrong
task, a profile that costs 280 % of its target, an in-kernel histogram at
12 %, two binaries with two privilege models -- and turns the CI green.

It runs as **twelve streams**, as many as the files allow: ten at once, then
two that need the first ten merged.

Read `CLAUDE.md` first -- **including "How much to test, and when", which this
wave is held to** -- then `STATE.md` ("What remains", "After the seventeenth
wave"), then the `docs/open-findings.md` sections your section cites, then your
section here. The earlier briefs are in `docs/briefs/`; the seventeenth's is
`docs/briefs/wave-17-programs.md`.

**This is the only brief.** What comes after this wave is written in
`STATE.md` under "What remains", and nowhere else.

---

## Decided before this wave

- **Which task the triage decomposes** (`DECISIONS.md`, "Open"): both of the
  two candidates the cases asked for, no `--thread` option. A wait for a
  child (`wait4`, `waitid`, a pipe read the child closes) is followed into the
  child that ran during it, and its decomposition takes the wait's place; and
  when the main thread's largest row is a block in which other threads of the
  process ran, the busiest of them is named beside it with its own
  decomposition. The verdict says which task each row is from. Chosen
  because it needs nothing from the person asking, which is what the triage
  promises.
- **One privilege model for `omn` and `omn-mcp`**: `omn-mcp` collects through
  the `omn` binary it ships beside, so the capabilities are granted to one
  file, by `make`, `make install`, `scripts/release.sh` and the sudoers line
  alike; and `tests/cases.rs` refuses to compare two ways run with different
  privileges rather than reporting a difference the machine made.
- **The phase 6 criterion stays open** until the coordinator's `null_blk`
  run of the corrected program statistics: stream io makes the statistics
  read the programs that ran, and says in its handoff which way the numbers
  point.
- **The CI installs bpftool**; it does not learn to expect its absence. A
  runner without swap skips the swap tests with that reason.

Each of these moves out of `DECISIONS.md` "Open" into the settled table, with
its reason, in the stream that lands it (by a decision line in its handoff).

---

## How you test, and how you read

- **While you work**: only your module and your test files --
  `cargo test --lib <module>`, `cargo test --test <file>`. Never `make check`
  while iterating.
- **Before your last commit**: one `make check`. If red, one
  `cargo test --workspace --no-fail-fast` to see every failure together. A red
  test outside your paths that is in the flaky tally (`docs/flaky.md`,
  `docs/open-findings.md`): rerun it alone once, and move on if it passes.
- **`cargo fmt` before `make`, never after.** `make` once per build, before
  eBPF tests.
- **Read by range**: `grep -n`, then the lines you need. Never read
  `docs/open-findings.md` or `STATE.md` whole: grep for your section.
- **Ten agents share this machine.** Assert the separation, not a threshold
  calibrated on a quiet one. Do not start `scripts/matrix.sh` or
  `scripts/bench.sh`.
- **Disk is short** (`/home` about 85 %): each worktree builds without
  incremental compilation (its own untracked `.cargo/config.toml`; do not
  commit it, do not remove it), and never copies a `target/` around.
- **Privileged commands**: `/etc/sudoers.d/omniscient-privileged` grants,
  without a password, exactly the commands in `STATE.md` "Needs privilege".
  **Do not change a sysctl and do not load a module**: they are machine wide
  and the other streams' tests read them. List the privileged run you want in
  your handoff, with the exact command; the coordinator runs it alone.
- **The CI logs** of the run this wave answers are in
  `../omniscient-ci-logs/37155975223/{defaults,stock}.log`.

## Where you work, and how you hand off

Each stream has its own worktree, `/home/moha/repos/omniscient-w18-<stream>`,
on `feat/w18-<stream>`. Commit there with `git commit`; never touch the main
checkout. The handoff is `docs/fixes/w18-<stream>.md`: what landed, what you
showed versus reasoned, what you could not do and why, the privileged runs you
want, your decision lines for `DECISIONS.md`, and the requests to other files
under "Needs from another stream". A request to a file you do not own is filed
there, not made.

---

## The streams

| Stream | Owns | Phase |
|---|---|---|
| **green** | `.github/**`, `src/collect/mem/{testing,swap,wss}.rs`, `tests/encoders.rs`, `docs/flaky.md` | A |
| **triage** | `src/triage/{latency,escalate,objective,engine,mod}.rs`, `src/triage/workload.c`, `tests/triage.rs` | A |
| **names** | `src/triage/{saturate,diff}.rs`, `src/cli.rs`, `docs/mcp.md`, `man/omn.1` | A |
| **mcp** | `mcp/**`, `Makefile`, `scripts/release.sh`, `docs/install.md` | A |
| **launcher** | `src/main.rs`, `src/collect/{mod,launch,task}.rs`, `tests/{budget,runner}.rs` | A |
| **profile** | `src/collect/cpu/profile.rs`, `src/unwind.rs`, `src/collect/mem/page_faults.rs`, `catalog/unwind.yaml`, `tests/{unwind,stack_capture}.rs`, `docs/unwind.md` | A |
| **sched** | `src/collect/sched/**`, `src/bpf/{sched_*,offcpu,runqlat,wakeup,switch_count,migrate}.rs`, `src/stack.rs`, `tests/{scenario,family}_sched.rs` | A |
| **io** | `src/bpf/{io,request}.rs`, `src/collect/io/**`, `tests/{scenario,family}_io.rs`, `tests/dm_delay.rs`, `docs/scenarios/io.md` | A |
| **engine** | `src/join.rs`, `src/event.rs`, `src/bpf/btf.rs`, `shim/**`, `tests/join_engine.rs`, `examples/**`, `docs/families/join.md`, `CATALOG.md` | A |
| **bench** | the `cost:` lines of `catalog/*.yaml`, `docs/bench.md`, `tests/bench_*.rs` | A |
| **resolver** | `src/resolve.rs`, and the test lines `1aa114f` moves | B, after io |
| **cases** | `cases/**`, `tests/cases.rs`, `docs/cases.md` | B, after triage, names, mcp, launcher |

A catalog entry's other lines belong to the stream that owns its collector.
`DECISIONS.md`, `STATE.md`, `ROADMAP.md`, `GUIDE.md` and the indexes are the
coordinator's. Phase B streams start from `main` with phase A merged.

### green -- the CI's two cells green

`docs/open-findings.md`, "The CI at `b37a925`, read back", and the flaky
tally ("The seventeenth wave, the coordinator's", the entries in files you
own).

- bpftool on the runner: `linux-tools-$(uname -r)` ships none for the azure
  kernel. Install it from a source that has it (Ubuntu's `bpftool` package
  where it exists, `linux-tools-generic`'s binary, or a build of the kernel
  tree's `tools/bpf/bpftool` pinned and cached), and make the step fail
  loudly when none answers `btf dump file /sys/kernel/btf/vmlinux`.
- "Configure the cell" hands `memory` (and every controller the tests ask)
  down to `/omn-ci/job`, so the limit tests run in `defaults`.
- The swap tests check for swap before setting their limit and skip with that
  reason when there is none; the zero-page swap test runs on the CI
  (sixteenth wave's filing).
- `wss::a_process_that_holds_much_and_touches_little_reports_the_little` and
  `encoders::a_known_allocation_lands_on_the_chrome_trace_...`: find why they
  fail on a runner, not just that they do; fix the test or the tool.
- You cannot run the CI: show each change on this machine where it can be
  shown (the skip gate, `.github/scripts/skip-gate.sh`, against a local log),
  and say in the handoff what only a run will show.

### triage -- which task a program's latency is

`STATE.md` item 3, first bullet, and "Decided before this wave".

- Follow a wait for a child into the child; name the busiest other thread
  when the main thread's largest row is a block others ran in; say which task
  each row is from. `make-sqlite`, `http-server-stdlib` and `gcc-shell` are
  the cases this turns (`docs/fixes/w17-build.md`, `w17-python.md`); hold it
  with a test in `tests/triage.rs` whose ground truth is a workload whose
  work is known to be in a child or another thread.
- `sched.mutex` is asked only when a sleeping row needs splitting, or through
  a backend under its own ceiling; a source's cost is said when it is over the
  criterion (python 5, build's mutex shim).
- `Limit::Sleep`'s sentence names a polled lock and a wait for a child.
- `--symbols` reaches every latency request that reads it (names owns the
  flag in `src/cli.rs`; you own where `latency.rs` honours it).
- `gcc-shell`'s return from `cases/held/` is the cases stream's, in phase B:
  say in your handoff whether it should.

### names -- what the verdict names

`STATE.md` item 3, last two bullets.

- `--symbols` on `latency` and `saturate`, set on every request that reads
  it; the verdict's names note.
- The hottest function under a perf map ranked by its innermost perf-map
  frame; a flat interpreter profile said as one; a block's stack printed to
  the program's own frame (python 1 to 3, `docs/fixes/w17-python.md`).
- `docs/mcp.md` and `man/omn.1` follow.

### mcp -- one privilege model, and what `run_session` answers

`docs/open-findings.md`, "`omn-mcp` carries no capabilities"; `STATE.md`
item 4's `mcp/` half.

- `omn-mcp` collects through the `omn` binary beside it ("Decided"), so
  `make`, `make install`, `scripts/release.sh`, `docs/install.md` and the
  sudoers line grant one file. The sudoers line in `CLAUDE.md` is the
  coordinator's: put the new spelling in your handoff.
- `run_session` answers with the program's stdout, and a waiting target's
  budget (a wall-clock denominator, or optional) is said, not guessed.

### launcher -- the budget and the stop

`STATE.md` item 4's `src/` half and item 11's registry line.

- The budget's denominator weighs the descendants the collectors watched.
- A stop ends the process group; a stopped run says so on its own line.
- `WEIGHS_ITS_SAMPLES` (`src/main.rs`) moves into the collector registry, so
  a collector declares it rather than a table naming it.

### profile -- what a profile costs, and the ladder

`docs/open-findings.md`, "a user-mode-only profile costs 280 % of the target";
`STATE.md` item 11's `mem.faults`, `unwind` and `collector_cpu_ns` lines.

- At `perf_event_paranoid` 2 without `CAP_PERFMON`, `cpu.profile` over
  `sqlite3`'s commit loop spends 266 % of the target's cpu. Find which mode the
  ladder picks there and why it costs that; bring it into its declared band
  or declare the band it has, and make the resolver's choice say it.
  Reproduce it without changing the sysctl: the capability-less binary
  (`cp target/debug/omn` elsewhere, no `setcap`) at this machine's paranoid
  level shows the user-only path, and the handoff says what only paranoid 2
  would add.
- The mode ladder where `mem.faults` is served; `unwind`'s stripped-build
  second shape; `collector_cpu_ns` for the profile.

### sched -- the sched collectors follow the target

`STATE.md` item 5, and `scenario_sched`'s fixed probe name (item 8).

- Follow the target's descendants as `cpu.profile` does.
- The 1.7 us a wait that leaves `blocked` for `runnable` is counted where it
  belongs.
- `sched.offcpu`'s stacks named through debuginfod (`src/stack.rs`), with
  "dynamic symbols only" said in the facts line when that is all there is;
  debuginfod held against a real server if one answers here, or a local one
  the test starts.
- `scenario_sched`'s probe name unique per checkout.

### io -- the histogram's cost, and the queue the CI cannot see

`docs/open-findings.md`, "the in-kernel histogram, measured", "`chmod` races
udev", and "the polled queue never sees the burst"; `STATE.md` item 11's io
lines.

- The program statistics read 0 runs while the histogram counted 26.8 M
  requests: read them from the programs that ran, while they are loaded.
  Then say, from the per-request numbers, whether the 348 ns is the programs
  or the dispatch (the open decision) -- the `null_blk` run itself is the
  coordinator's, with your exact command.
- Wait for udev before the `chmod` of `/dev/nullb0`.
- The polled queue depth that reads 0 on the runner's nvme while `inflight`
  reads 199: find what it reads instead (hardware queues, a partition, the
  wrong device) and fix the source, not the test.
- A known set of device latencies (`null_blk` with a fixed `completion_nsec`)
  for the edge-crossers, with its test; driving a known number of merges; one
  program for both `io.latency` histograms if it falls out of the rest.

### engine -- the files nobody owned

`STATE.md` item 6, item 8 (except `tests/cases.rs`, `docs/cases.md` and
`scenario_sched`), item 11's engine lines.

- `shim/src/lib.rs:384`: no clock read on a call that does not report.
- `btf.rs`'s `member_offset` follows a typedef (a shared layer: test every
  caller).
- `examples/cross-machine-reclaim-io.yml`'s `enclosing:` line with its
  assertion (one measured run first); `tests/join_engine.rs`'s
  other-namespace test; `docs/families/join.md:39`; `docs/unwind.md`'s and
  `CATALOG.md`'s perf-map line under `--symbols local` (`docs/unwind.md` is
  profile's: file it there, write `CATALOG.md`'s).
- A rule choosing the kernel or query tier (phase 5's last `[~]`);
  `assemble`'s linear clone in `src/join.rs`.

### bench -- the bands, corrected to the table

`STATE.md` item 9; the table is `target/bench/backends-1791030430` in the
main checkout (read it there, do not copy it).

- Correct each `cost:` the table measured outside its band -- 46 pairs, worst
  `io.fsync` through `bpf:fentry` at 142 % -- to the band it measured, or say
  in `docs/bench.md` why the declared one stands (a workload that is not the
  source's case). An in-kernel collector's cost is the target's dilation, not
  the tool's cpu.
- `tests/bench_*.rs` hold the catalog to the table it last measured, so a
  band can no longer drift from the measurement silently.

### resolver -- one order (phase B)

`STATE.md` item 7, `docs/fixes/w17-core.md` section 2.

- Cherry-pick `1aa114f` with the eleven test changes in one merge; then
  `scenario_io`'s two rows and the pinned `net.hwts` twin. The exit-3 line of
  `DECISIONS.md` goes in your handoff, worded.

### cases -- the cases, re-asked (phase B)

`STATE.md` item 8's `tests/cases.rs` and `docs/cases.md` lines, and the
privilege half of "Decided".

- `says:`, `held:`, the perf-map ranking in `read_mcp`, the wait for a
  stopped run's descendants; `docs/cases.md`'s injection table.
- `tests/cases.rs` refuses to compare two ways run with different privileges.
- Every case re-asked on merged phase A: each case's first suspect for its
  objective, both ways; `gcc-shell` back from `cases/held/` if the triage now
  names its cause; the flaky tally's two `cases` lines removed if python's
  findings 2 and 5 are what removed them.

---

## Exit criteria for the wave

- The CI's two cells green, or red only on what a run shows and this machine
  cannot, filed with the log line.
- `make-sqlite`, `http-server-stdlib` and `gcc-shell` name their known cause
  first, both ways, or are filed against what still gets it wrong.
- `cpu.profile` at paranoid 2 within its declared band, or the band says what
  it costs.
- The program statistics read the programs that ran; the phase 6 criterion
  decidable from the coordinator's run.
- One privilege model for both binaries.
- The bench's bands match the table, and a test holds them to it.
- Items 6 to 11 of "After the seventeenth wave" landed or re-filed with what
  blocked them.
- `make check` green at each merge.
