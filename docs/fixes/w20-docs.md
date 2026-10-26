# The twentieth wave, stream docs

Items 10 (the documents that lag the code, the part this row owns) and 11
(the flaky tally's entries in files nobody else owns) of `docs/dev/STATE.md`. Branch
`feat/w20-docs`, on `main` at `777d6dd` merged in.

## What landed

- **`35e167a` docs(catalog): the cost column and the sched section follow
  the catalog.** Every family table's Cost cell is now what `catalog/*.yaml`
  declares: the chain's tiers in chain order, without repeats (`low / med`
  is the first backend `low`, a fallback `med`). 49 rows changed, among them
  every one `docs/fixes/w18-bench.md` listed (`cpu.cycles` ... `free` to
  `low`; `io.size`, `io.fs`, `io.fsync` to `high`; `net.tcp.rtt` to `med`;
  `power.cstate`, `gpu.freq` / `.power` / `.temp`, `proc.exec` / `.exit`, the
  `mem.*` smaps rows) and some it did not (`mem.heap.*`, `sched.mutex`,
  `proc.syscall` are `extreme`; `sched.futex` `high`). A paragraph under the
  cost scale says what the column is and that the YAML is the authority. The
  sched section's Backends and Targets columns now name the raw tracepoint
  first, as the chains do, and the caveats say that a process target is
  followed into its children (offcpu, runqlat, wakeup, switches, migrations,
  lock, futex) and that a run-queue wait is `sched_info`'s account, with
  `wake_to_run_ns` beside it.
- **`518328c` docs(examples): `proc-not-built` exits 3, or 1 where tracefs
  is root-only.** The header said 3 everywhere; `force:` skips the
  resolver's choice but not the pinned backend's probes, and a missing
  requirement is refused before a missing collector. `docs/GUIDE.md` and
  `examples/README.md` say both. `docs/GUIDE.md`'s exit-code blockquote ("two of
  those four are wrong") was stale: both defects (`docs/scenarios/io.md`, 1
  and 3) are fixed; it now says so.
- **`17e20c1` docs(guide): the latency session.** `docs/GUIDE.md`'s triage
  section says which four sources the session asks, that `sched.mutex` is
  only in a second pass when the verdict reads a futex sleep, that the
  verdict follows a wait into a child or the busiest thread (`followed`),
  and the 20 % cost line. `triage-latency-lock.yml`'s row says the two
  passes.
- **`3258713` docs(unwind): the perf map.** `docs/unwind.md` section 4 lists
  `PerfMap { lines }` among the module origins and says the perf map is read
  under `--symbols local` (and so under `debuginfod`), with `NamedBy::PerfMap`
  (`perf-map`) on each frame it named.
- **`e15c418` fix(bench): `judge_band` reads a dilation within the
  workload's resolution as zero.** A new `resolution: Option<f64>` argument
  (the gate's `Stats::resolution`, a fraction); a dilation no larger than it
  is not counted, and a row with no share whose dilation is within it is
  judged at 0 % with the reason. `tests/bench_backends.rs` (no stream's file;
  the only caller) passes the row's workload's baseline resolution, so the
  table's `band` column and `tests/bench_bands.rs` now agree.
  `tests/bench_bands.rs`'s module comment and `docs/bench.md`'s "one change"
  paragraph, both no stream's, now say the harness applies the floor too.
  New test: `a_dilation_within_the_workloads_resolution_reads_as_zero`.
- **`77d5205` test: two flaky bounds, and the cases ask with no budget.**
  - `tests/cases.rs`: `MODEL_BUDGET`, `UNBOUNDED_BUDGET`, the "`budget` is
    required" fallback, `Asked::OverBudget`, the re-ask and its `sweep`, and
    the check `the-agent-s-way-answers-within-a-model-s-budget` are gone
    (it could only be "unchecked" since `run_session` stopped requiring a
    budget). `ask_mcp` asks with no budget. Stream budget's request: what
    the agent's way reads `from: target` is now `said_by_omn` followed by
    the whole `program_stderr` file, where `run_session` puts the program's
    stderr. `docs/cases.md` (no stream's) loses the check's row and says
    where the program's account is read; `examples/mcp/approved/program.py`'s
    header says `program_stderr`.
  - `scenario_mem::the_two_roads_to_a_footprint_arrive_at_the_same_number`:
    the two roads are read one after the other each tick, and the round
    ends when a collector finds the workload gone (`src/collect/mod.rs`,
    `read`), so the road read second can have one reading fewer. The test
    now allows one, and compares the last readings only when both ended on
    the same tick; the zero check (D1) is unchanged.
  - `collect::mem::page_faults::tests::sampled_in_user_mode_the_weighed_samples_are_the_faults_the_thread_took`:
    the weighed in-block samples sat at the bound's edge (4064 or 4080
    against 4096 taken, bound 32). A period that ended on a fault outside
    the block carries up to `PERIOD` of the block's faults no in-block
    sample weighs; the shortfall bound now adds one period per sample that
    landed elsewhere (5 or 6 per run here), and the message prints them.

## Shown versus reasoned

- Shown: the cost column against the YAML (a script over `catalog/*.yaml`
  and `docs/CATALOG.md`, every row that names its metrics compared;
  `mem.cache_*` fixed by hand). `proc-not-built.yml` exiting 1 here, naming
  the unreadable tracefs for each of the four (built binary, no
  capabilities, then the same with them). `omn latency --objective latency
  --dump-session` listing exactly `cpu.profile`, `sched.runqlat`,
  `sched.offcpu`, `sched.futex`. `judge_band` by its new test.
  `tests/cases.rs`: the agent's way reading `account.*` and the program's
  references out of `program_stderr` on `cpython-perf-map` and
  `sqlite3-durability` (both causes each, with the capabilities): every
  `from: target` reference parsed. The page-fault margins (15 runs: 4096,
  4080, 4064 against 4096, then 5 to 6 samples elsewhere).
- Reasoned: `proc-not-built.yml` exiting 3 on a machine with a readable
  tracefs (not available here). The footprint test's one-reading race (from
  the read loop; not reproduced in 10 alone runs). That the page-fault
  test's out-of-block samples are the ones that cost the in-block weight
  (the bound is sound either way; the attribution is not measured).

## Not done, and why

- **`scenario_runtime::a_probe_follows_every_thread_of_the_process`**
  (no stream's file): reproduced once in 40 alone runs, with load from my
  other runs: `{..000: 501, ..005: 499, others 500}`, 4000 in all, the tids
  contiguous (no reuse). The tid comes from `bpf_get_current_pid_tgid`
  into a record on the program's own stack and out through the ring
  (`src/bpf/usdt.rs`), and the ring is mapped twice so a record across the
  wrap is whole; I found no path by which one hit could carry another
  thread's tid. **Shown on a second reproduction** (run 85 of a loop of
  `omn -f examples/runtime-usdt-threads.yml --format json`, the events read
  by their `arg0`, which is the loop index `i`): thread `..762` has 499 hits
  and lacks `i = 90`; thread `..764` has 501, two of them `i = 90`. One is
  at `..024306718`, between its own 89 (`..304458`) and 91 (`..308448`).
  The other is at `..024161985`, between `..762`'s 89 (`..144454`) and 91
  (`..199806`), in the 55 us gap where `..762` was off its cpu. So
  `..762`'s hit came out of the kernel carrying `..764`'s tid, and the
  program writes that tid first, from `bpf_get_current_pid_tgid`. The cause
  is upstream of anything `omn` decodes: a uprobe handled while `current`
  was another thread of the same process (kernel `7.2.7-arch1-1`,
  `PREEMPT_DYNAMIC`). That is a reasoned guess, not something I have shown.
  Not fixed: the test is right to fail. Its message could say "a hit
  attributed to another thread of the process", and a counter that
  compares each hit's `arg0` sequence per thread would detect it in the
  collector. Filed for whoever owns `runtime` next. One in 40 to 85 runs
  here.
- **`cases`** (already in the tally): `cpython-perf-map`'s
  `every-cited-number-agrees-with-the-reference` failed twice, once each way
  (mcp: on-cpu 1680.0 ms against `account.cpu_ns` 1639.4 ms, 40.7 ms over a
  35.9 ms allowance; cli alone: 41.4 ms over 40.1 ms). The excess is always
  the profile above `process_time`. A time-based sample also counts the
  interrupts on the task's cpu, which `getrusage` leaves out, and the case
  allows 1 % for that; a loaded machine has more interrupts. Reasoned, not
  measured: not fixed.
- `tests/cli.rs`'s
  `a_refusal_for_an_unwritten_backend_leads_with_the_one_passed_over` failed
  once in `cargo test --test cli` while my repro loop was running the
  binary and passed alone. Not mine (stream shared's file).

## Privileged runs wanted

None beyond what `make` grants. To confirm `proc-not-built.yml`'s exit 3,
on a machine whose tracefs uid 1000 can read (not this one): `make &&
target/debug/omn -f examples/proc-not-built.yml; echo $?`. The cases, alone
on the machine: `make && cargo test --test cases -- --nocapture`.

## Decision lines

- **`docs/CATALOG.md`'s Cost column is the YAML's chain, written out.** It lists
  the tiers each chain declares, in order. If it is left open, the column
  drifts again, as it did after wave 18. The next step is a test reading the
  column against the catalog (filed below).
- **The cases' budget check is gone rather than kept unchecked.** A check
  that cannot pass is noise in every report. If a budget is wanted again,
  the check comes back with a ceiling the case states.

## Needs from another stream

- **shared (`tests/cli.rs`, beside the index tests)**: a test that reads
  each family table's Cost cell in `docs/CATALOG.md` and holds it to the
  catalog's chain (the rule above; `mem.cache_l1` / `.llc` / `.tlb` is the
  one grouped row whose ids do not share a prefix).
- **Coordinator (`docs/scenarios/proc.md:36,161,182`, `docs/families/proc.md`)**:
  `proc-not-built.yml` is exit 3, or 1 where tracefs is root-only (as
  `docs/GUIDE.md` now says).
- **Coordinator (`docs/open-findings.md`)**: the tally entries above for
  `scenario_mem` and `page_faults` are fixed by `77d5205`; `scenario_runtime`
  and `cases` stay, with what this handoff found.
- **Coordinator (`docs/CATALOG.md` has no `net.request` row)**: section 5 does not
  list `net.request`; the runtime section lists only USDT probes. Adding the
  row is a one-line change I left alone because its wording belongs to net.
