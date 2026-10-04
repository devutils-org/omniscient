# The twentieth wave, stream cpu

Exact on-cpu time per thread, and `raw` (STATE.md items 8 and 2).

## What landed

- **`cpu.oncpu_time` through `bpf:raw_tp`** (`src/bpf/oncpu.rs`,
  `src/collect/cpu/oncpu_bpf.rs`, registered in `src/collect/cpu/mod.rs`, and
  one `mod` line plus one `pub use` line in `src/bpf/mod.rs`). A raw
  tracepoint program on `sched_stat_runtime` adds up, per thread, the
  increments `update_curr` makes to `se.sum_exec_runtime`. That is the number
  `/proc/<tid>/schedstat` publishes as its first field. The rows are drained
  every poll: one event per thread that ran, keyed `task:<tid>`, with `ns`
  (the value), `updates`, `pid`, `tid`, `comm`, `window_ns` and
  `resolution_ns: 1`. No clock is read. A process target follows its
  descendants (`sched_common::Descendants`), the way `sched.switches` does.
  The request's `comm` filter is applied per drained row. Losses are counted:
  skipped runs, a full row map, unfollowed descendants, and rows that vanished
  or were truncated in the drain.
- **The first version used `sched_switch`, and I replaced it before
  committing.** That version credited a slice only when the slice ended. A
  thread spinning alone on a pinned core was never switched out, so it read
  **nothing** for the whole 400 ms window: that was
  `a_core_with_one_tenant_reads_the_same_through_two_files`, run on this
  machine. `sched_stat_runtime` fires on every scheduler tick of a running
  task as well as on the switch, so a running thread is credited within one
  tick. Each edge of the window is at most one tick off. The catalog caveat
  says so.
- **Catalog** (`catalog/cpu.yaml`): the `bpf:raw_tp` line now requires
  `raw_tracepoint:sched_stat_runtime(task_struct *, u64)` plus the fork and
  free tracepoints the descendant follower needs, and `kernel>=6.1`. It has
  `modes: [poll]` and `drained: true`, and its targets are system, cgroup,
  pid, tid and launch; a container stays with `fs:cgroup`. Two caveats were
  added: rows are per thread where `fs:proc` gives a process total, and each
  edge of the window is off by at most one tick. A unit test holds the
  prototype to the running kernel and to the catalog line
  (`mem_program::assert_catalog_lists`).
- **`raw`** added to `cpu.oncpu_time` and `power.energy`. `cpu.util` already
  declared and served it; I checked `--system -e raw` live. `raw` is generic
  in `emit.rs`, so the catalog line was the only missing piece.
- `fs:proc`'s note and doc now name `bpf:raw_tp` as the backend that resolves
  below a tick. Its trace refusal still names `bpf:tp`, which is true: nothing
  serves `trace`.

## Shown versus reasoned

Shown on this machine (kernel 7.2, with the capabilities applied by `make`):

- `tests/scenario_cpu.rs::a_threads_rows_add_up_to_what_the_scheduler_credited_it_to_the_nanosecond`:
  a shell is blocked when its `schedstat` is read before the run and blocked
  again when it is read after. It spins in between. The shell's own rows sum
  to the `schedstat` difference **exactly** (`assert_eq!`, no tolerance). It
  passed with both program versions.
- `a_launched_programs_rows_add_up_to_its_reaped_cpu_time`: the rows fall
  short of `wait4`'s `cpu_ns` by 47 to 68 us. That is what the forked child
  ran between the fork and the gate, before the program was attached. The
  test asserts a gap of -2 us to 1 ms against a spin of over 100 ms.
- A system run of 1 s gives 195 per-thread series and no loss.
- `scenario_power::the_raw_events_of_a_run_are_the_readings_its_timeline_is_drawn_from`:
  every raw `power.energy` event carries its domain, and the raw events sum
  to the timeline's points.

Reasoned, not shown:

- The edge error is at most one tick per edge. I did not measure it in
  isolation.
- Before 6.8, `sched_stat_runtime` had a third argument. The prototype check
  refuses those kernels with the reason, and the resolver falls back to
  `fs:proc`. I did not run it on such a kernel; that is the matrix.
- Cost: the program runs once per tick per running followed thread, plus
  once per switch. I did not measure it (`bpf_stats_enabled` is 0 here). The
  catalog says `low`, like `sched.switches`.

## Behaviour change other streams will see

Wherever the binary carries `CAP_BPF` and `CAP_PERFMON`, `cpu.oncpu_time` now
resolves to `bpf:raw_tp` before `fs:proc`. That means **one series per
thread** (`task:<tid>`) in place of one per process (`task:<pid>`), and
nanoseconds in place of 10 ms ticks. Anything that reads `series[0]` as "the
process" has to sum the series. I fixed my own tests that did this
(`consumed_cpu` in `scenario_cpu.rs`). Two tests whose subject is `/proc`'s
tick are now pinned with `--backend force:fs:proc`. I also ran these, with
the capabilities: `runner::the_reap_carries_the_cpu_the_last_poll_missed`,
`cross_compute` (8 tests), `view_live` (5), `cli` (26) and `triage` (27).
None of their failures is from this change; see "The suite".

## What I could not do, and why

- **`trace` mode** (one event per slice) is not served. The backend declares
  `modes: [poll]`, so a `trace` request falls through to `bpf:tp`, which has
  no collector, and is refused before launch as the build's gap. A per-slice
  stream would need `sched_switch` and a ring, and nothing asked for it this
  wave.
- **`CATALOG.md`** and `docs/scenarios/cpu.md` are not my files. See the
  requests below.
- I did not add a note when the poll is faster than the scheduler tick (a
  running thread's readings come in steps of one tick). The caveat states it.
  The tick (`CONFIG_HZ`) cannot be read without the kernel config.

## Privileged runs wanted

None beyond `make`, which grants the capabilities and is what these tests
used. To time the program inside the target:

```console
sudo -n sysctl kernel.bpf_stats_enabled=1   # not in sudo -n -l here; for whoever runs the bench
target/debug/omn cpu.oncpu_time --system -m poll@100ms -e summary -d 10s
sudo -n sysctl kernel.bpf_stats_enabled=0
```

The matrix (alone, at the end of the wave) should show the `fs:proc` fallback
on a kernel older than 6.8, where the prototype has three arguments.

## Decision lines for DECISIONS.md

- `cpu.oncpu_time` per thread through `sched_stat_runtime`, not
  `sched_switch`: the scheduler's own increments, credited every tick, so a
  thread that is never switched out is not invisible; exact against
  `schedstat` over a window whose edges find the task blocked.
- The `bpf:raw_tp` backend of `cpu.oncpu_time` serves `poll` only. `trace` (a
  slice per event) stays unbuilt until a view asks for it.

## Needs from another stream

- **docs**: `CATALOG.md`'s `cpu.oncpu_time` row: the `bpf:raw_tp` backend on
  `sched_stat_runtime`, poll only, per thread; `raw` on `cpu.oncpu_time` and
  `power.energy`.
- **coordinator**: STATE.md item 8 (the `bpf:raw_tp` collector; `raw` on
  `power.energy` and `cpu.oncpu_time`) and item 2 (`raw` on `cpu.util` and
  `cpu.oncpu_time` for the views) can be struck. `docs/scenarios/cpu.md` could
  record the `sched_switch` version's blind spot as a finding the scenario
  caught.
- **view / record**: the ledger and system views that read
  `cpu.oncpu_time` will now get per-thread series on a privileged machine. A
  view that wants the process total sums the series, or groups them by the
  `pid` attribute.

## The suite

- `cargo test --lib -- oncpu catalog resolve validate`: all pass.
- `cargo test --test scenario_cpu`: 42 passed. `--test family_cpu`: 17
  passed. `--test family_power`: 15 passed. `--test scenario_power`: 18
  passed.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
  `cargo fmt --check`: clean.
- Outside my files: `triage::the_same_program_is_cpu_bound_for_latency_and_sleep_bound_for_the_tail`
  failed in the full triage file and **again when rerun alone**. The verdict
  was "no single state is decisive", with 217 ms the rows cannot speak for.
  It is already in the flaky tally (`docs/open-findings.md`). Its methods use
  `cpu.profile` and `sched.offcpu`, not `cpu.oncpu_time`. It failed twice, so
  it is filed for the triage stream to read. `triage::a_contended_mutex_is_what_the_workload_queues_for`
  failed once in the file and passed alone.

## Review

Two findings, both gaps, both fixed: a skip now has to be the machine's.

- **`tests/scenario_cpu.rs`, `oncpu_rows`**: a refusal skipped through
  `machine_s_reason`, which only lays the message out. It now asserts
  `machine_cannot_serve(&message)` first, as the file's other skips do, so a
  refusal of the request itself (`-e raw`, `poll@100ms`) fails both
  `bpf:raw_tp` tests instead of skipping them.
- **`tests/scenario_power.rs`,
  `the_raw_events_of_a_run_are_the_readings_its_timeline_is_drawn_from`**:
  any non-zero exit was called "this machine does not serve power.energy". It
  now asserts the stderr says `no viable backend` or `cannot start
  collecting` before skipping.

Shown, not reasoned: each guard was mutated (`-e raw` changed to `-e rawx`)
and the three tests failed with "refused rather than unserved: ... unknown
emit `rawx`"; restored, all three pass after `make` with no skip line
(`--nocapture`). Tests run: those three, plus clippy and fmt, clean.
