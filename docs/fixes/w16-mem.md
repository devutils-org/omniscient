# The sixteenth wave — stream mem

Branch `feat/w16-mem`, from `main` at `37f17c8`. The files: `src/collect/{mem,sched,proc,net}/**`
(not `mem/heap/**`), `permission_advice` in `src/bpf/mod.rs`, `net_probe` in
`src/bpf/attach.rs`, the `mem.*`, `sched.*`, `proc.*` and `net.*` catalog
entries, those families' scenario and family tests, their sessions and
documents, and `docs/flaky.md`.

## 1. The four CI reds

### `scenario_mem::every_mode_and_emit_of_the_counter_sources_is_served_or_refused_by_the_right_code` (defaults): a tool defect

The `defaults` cell refused ten sampled and traced `mem.faults` combinations
with exit 1: *"perf_event_open for page faults on pid 14345 was refused
(perf_event_open: Permission denied (os error 13))"*. The fifteenth wave added
a retry in user mode only for paranoid 2, and the retry never ran. It matched
the refusal by `raw_os_error()`, and `crate::perf` returns the errno wrapped in
a message (`io::Error::new(os.kind(), format!("perf_event_open: {os}"))`),
which keeps the kind and drops the number. The library test of that path
passed on the same cell because it forced the path with a bare errno, a shape
the real path never produces.

**Fix** (`src/collect/mem/page_faults.rs`): the retry matches
`ErrorKind::PermissionDenied` (`is_refusal`), and the forced path is taken with
an error in the shape `crate::perf` returns (`refused_as_perf_says_it`), so the
forced path and the machine's path are one path. Recorded as D14 in
`docs/scenarios/mem.md`.

**Test**: `collect::mem::page_faults::tests::a_refusal_is_recognised_in_the_shape_perf_returns_it`.
It opens the real event on pid 1, which belongs to root and which the kernel
refuses to an unprivileged user at every paranoid level (`ptrace_may_access`),
and holds `is_refusal` to that error. It ran for real here (no skip line).

The two scenario tests the fix lets run on `defaults` for the first time
compared against `ru_minflt`, which counts the faults the kernel took for the
program as well. They now read the run's own note (`notes`: *"page faults in
user mode only"*, `sampled_in_user_mode_only`) and, when it is there, hold the
run to what the program's own stores take:

- `the_faults_a_worker_thread_takes_are_traced_to_that_thread`: at least the
  16 384 pages the worker stored into (a mapping advised against huge pages),
  and at most its own `RUSAGE_THREAD` count plus its tail;
- `the_tool_finds_the_faults_the_program_counted_itself`: the same comparison
  with `ru_minflt` on both paths, a shortfall under 10 %, widened to 15 % where
  the run says it sampled in user mode only (the kernel's own faults for this
  workload: 17 of 1610, measured with `perf stat -e
  page-faults:u,page-faults:k`). The first version kept only a floor of 128 on
  that path; the review showed the exact check still served (below). And the
  test had skipped on every machine since the fifteenth wave, which the
  review's run made visible: D16.

### `collect::mem::pressure::tests::a_cgroup_forced_to_reclaim_stalls_and_an_idle_one_does_not` (defaults): the test's

`memory.max` at 24 MiB with 512 MiB touched had the child OOM-killed on the
CI's swap file (`memory.events`: `max 1631 oom 1 oom_kill 1`). The cell is the
first to run this test at all: only `defaults` delegates a cgroup. **Fix**:
`memory.high` instead. Both make the kernel reclaim from the cgroup as it
grows, and the waiting is the stall measured; only `max` calls the OOM killer
when reclaim falls behind, and over `high` a task that outruns reclaim is
throttled, which is itself counted as memory stall.

The throttling is up to two seconds per charge batch, so on a disk too slow
for reclaim the child can still be touching when the toucher's thirty-second
handshake gives up -- a different red, not a green (the review's point 2). So
the child is not required to finish: `Scratch::spawn_throttled_toucher` returns
after the work or after thirty seconds, says which
(`Toucher::finished_touching`), and the test reads the window either way,
because thirty seconds of throttling is thirty seconds of stall. And it touches
192 MiB rather than 512 (eight times the limit), which asks a third of the
swap of the cloud disk. A child that *dies* still panics with its
`memory.events`. Passed eight times here (load 16 to 96).

### `collect::mem::swap::tests::a_page_of_zeros_is_swapped_without_being_written` (defaults): hardened, not proven

*"memory.swap.current said 23486464"* of the 160 MiB that had to leave the
cgroup. The same test here moves `swpout_zero` by 41 130 pages (160 MiB) and
`thp_split_page`, `thp_underused_split_page` and `pswpout` by nothing. The
cause on the CI is not known. What was changed:

- the zero-filled toucher maps small pages (`MADV_NOHUGEPAGE`,
  `src/collect/mem/testing.rs`): a huge page of zeros split under reclaim can
  have its zero subpages remapped to the shared zero page and freed
  (`thp_underused_split_page`, 6.12 on), which takes them out of the cgroup
  without a swap entry, and the CI kernel is 6.17 with an unknown THP mode;
- the failure now carries `memory.current` and the deltas of `pswpout`,
  `swpout_zero`, `thp_split_page`, `thp_underused_split_page`, `thp_swpout`
  and `thp_swpout_fallback` over the run, so the next red says which.

### `scenario_net::the_trace_leg_of_the_sweep_answers_or_names_the_backend_it_would_need` (both cells): the test's

The third leg (`net.hwts -m trace -- true`) expected exit 3, *"no collector is
written"*. On both runners the chain path rejects `so_timestamping` first for
its other requirement, `phc`: the runner keeps `/dev/ptp1` (Hyper-V's card),
root's alone, so exit 1 with *"so_timestamping: phc: missing: ... /dev/ptp1
could not be read (Permission denied)"*. That is the machine's answer, and a
true one. The test now decides which machine it is on independently of the
tool -- the interfaces' clocks as sysfs links them
(`/sys/class/net/<iface>/device/ptp/ptp<N>`), each opened here
(`no_readable_ptp_clock`) -- and asserts exit 1 with the backend and the clock
named where none opens, exit 3 with "no collector is written" where one does.
Which of the two answers wins when both are true is the resolver's, filed (see
below).

## 2. `permission_advice` stops advising a grant already made

`src/bpf/mod.rs`: `permission_advice` asks
`capabilities::withheld_file_capabilities()` first and, where the binary
carries the capabilities and the process was not given them, says why
(`nosuid`, `no_new_privs`, a grant without `e`) instead of the `setcap`. The
composition is `advice_for(action, sysctl, withheld)`.

**Test**: `bpf::tests::the_advice_says_why_a_grant_did_not_reach_the_process_rather_than_to_grant_it`.
The withheld branch is held to the composition only: an in-process test
binary carries no file capabilities, and the attach is reached after the probe,
which already says the same since the fifteenth wave. End to end, it is the
`nosuid` item under "Needs privilege" in `STATE.md`.

## 3. `net_probe` is gone

`collect::mem::net_probe` removed; `bpf::attach::probe_type_support` calls
`crate::collect::net::probe(kind)`. Covered by the `net.hwts` tests above and
`scenario_net` as a whole (the `phc` and `ethtool` probes go through it).

## 4. The shim check matches what the tool prints

`tests/family_mem.rs`, `tests/scenario_mem.rs`: `SHIM_MISSING = "shim was not
found"` (the tool prints *"the preload shim was not found at ..."*);
`tests/cross_memory.rs:130` the same string. **Test**, in each of the first two:
`a_binary_without_its_shim_fails_the_heap_tests_rather_than_skipping_them`
copies the binary into a directory with no shim beside it or in
`../lib/omn/` (checked), runs `mem.heap.alloc` with `OMN_PRELOAD_LIB` unset,
asserts the tool's words are there, and that the file's check panics on that
output rather than reading it as a machine limit.

## 5. Documents

- `docs/scenarios/net.md`, defect 7: what the code does now -- exit 2 for a
  `trace` of an interface (the validator: no backend declares the pair), exit 3
  for a launched program where a clock is readable, exit 1 where none is.
- `docs/scenarios/sched.md`: the loss path is the futex burst (C fixture
  `wakers`) checked once, and the moderate-rate test beside it; the Python
  session stays the demonstration, and the workloads list says which is which.
- `docs/families/sched.md:57`: `sched.lock`'s backends require `kernel>=6.1`;
  there is no floor for the tool (`DECISIONS.md`, the fifteenth wave).
- `docs/flaky.md`: #3 closed (the row and its paragraph), and a section for
  this wave's reruns.
- `docs/scenarios/mem.md`: D14.

## 6. The flaky tally entries, alone, five times each (and what failed later)

35 of 35 passed, at load average 2 to 5: `scenario_mem::both_huge_page_backends_read_the_same_promotion_counter`
(#21), `a_gibibyte_of_reads_costs_one_l1_miss_per_cache_line`,
`a_walk_that_fits_in_l1_is_never_timed_and_one_that_does_not_waits_on_memory`,
`family_mem::the_working_set_is_what_was_touched_and_not_what_was_held`,
`scenario_proc::defect_1_...`, `defect_2_...`,
`scenario_sched::a_forced_switch_program_splits_the_switches_as_the_kernel_counts_them`.
None failed again then. Later the same day, at load 16 to 96, #21 failed
alone five rounds in a row with one gap -- a tool defect, fixed (D15, #21
closed) -- and the cache test failed in its own file's run, a calibrated bound
replaced by a separation; `defect_1` failed once in the whole-workspace run
and passed alone once. `docs/flaky.md` has both tables.

## The review's six points

1. **The user-mode path dropped the exact comparison** -- real. Restored, 15 %
   on that path (above). Locally it runs the 10 % bound at paranoid -1; it
   could not be run at paranoid 2 here (changing the sysctl is outside this
   stream's privilege). And the test could not have caught either version:
   `unavailable` read `perf_event_paranoid` in the caveat every run prints and
   skipped. Fixed, D16; all 42 `scenario_mem` tests still pass, now with that
   comparison run.
2. **Pressure under `memory.high` could time out instead** -- real, as reasoned.
   Fixed: a throttled child is accepted, 192 MiB touched (above).
3. **The zero-page fix is a guess** -- agreed, and the table below now says
   *unknown* rather than *expected green*. Nothing in the logs gives the
   runner's THP mode; the `MADV_NOHUGEPAGE` stays (harmless, and removes the
   one mechanism known to take zero pages out of a cgroup without a swap
   entry), and the failure message's vmstat deltas are the evidence the next
   red brings.
4. **The cache test fails in its own file** -- real; reproduced (72296 TLB
   misses with all 131072 KiB of the buffer on huge pages, so not a THP
   fallback). Fixed as a separation (`docs/flaky.md`): two scans, advised onto
   and off huge pages, `tlb_scan_session`. Three file runs and three alone
   green since.
5. **`faults_are_reported_at_the_address_that_faulted` red in the module** --
   real, and the bound was wrong rather than tight: it counted events, and the
   window opened before the mapping. Fixed (`docs/flaky.md`); five runs green.
6. **Green runs** -- no change. One addition the same runs found: the new
   no-shim test hit `ETXTBSY` once in its file's parallel run; retried.

## Also landed: the interface index (stream docs' request, the fifteenth wave)

The first bullet of *"`iface` is compared by its name alone"* was in this
stream's files: `net.bytes`/`net.packets` (`dev.rs`) and `net.iface`
(`procfs.rs`, `iface.rs`) wrote index 0. They now ask `if_nametoindex` at the
read. **Tests**: `collect::net::dev::tests::every_series_carries_the_index_sysfs_gives_its_interface`
and `collect::net::iface::tests::every_series_carries_the_index_sysfs_gives_its_interface`,
against `/sys/class/net/<name>/ifindex`. The other two bullets (`identity`
comparing the index, the counter kind out of `net.iface`'s name) are
`src/join.rs`'s and a surface change, not this stream's.

## Checks

After the review (the disk was full again, 4 KiB to 820 KiB free on `/home`;
built and run with `CARGO_TARGET_DIR=/tmp/claude-1000/w16-mem-target`,
`CARGO_INCREMENTAL=0`, so without capabilities): `cargo test --lib --
collect::mem collect::net collect::sched collect::proc bpf::tests` 344 passed;
`scenario_mem` 42 passed (five whole-file runs after the last fix),
`family_mem` 16, `cross_memory` 9, `cli` 26. The commit's check is below the
first run's notes: `cargo test --workspace --no-fail-fast` with the same
target directory and `CARGO_PROFILE_{DEV,TEST}_DEBUG=line-tables-only` (full
debuginfo hit the `/tmp` quota mid-link) -- **2122 passed, 11 failed, 10
ignored**; every failure is a test that runs `<checkout>/target/debug/omn` by
that path (`bench_capabilities` x3, `portability::the_listing_parser_reads_the_real_listing`,
`omn-mcp` x7), which does not exist while the worktree's own `target/` is
deleted to keep `/home` writable. None in this stream's files; clippy
(`--all-targets -D warnings`) and `cargo fmt --check` clean.

Before the review:

- While iterating: `cargo test --lib` for `collect::mem::page_faults`,
  `collect::mem::swap`, `collect::mem::pressure`, `collect::net`, `bpf::tests`;
  `tests/scenario_mem.rs`, `tests/scenario_net.rs`, `tests/family_mem.rs` whole,
  in the worktree after `make` (capabilities on): 42, 40 and 16 passed.
- `make check` in the worktree: **failed twice before any test ran**, at link
  time, *"No space left on device"* -- `/home` was at 0 to 600 M free (see
  "Open").
- So the check was run with `CARGO_TARGET_DIR=/tmp/claude-1000/w16-mem-target`
  and `CARGO_INCREMENTAL=0`: `cargo test --workspace --no-fail-fast` **2129
  passed, 4 failed, 10 ignored**; clippy (`--all-targets -D warnings`) clean,
  `cargo fmt --check` clean. The four, none in this stream's files:
  - `collect::cpu::profile::tests::a_profile_of_the_machine_finds_this_thread_for_the_cpu_it_burned`
    (*"262 of 199"* samples, a machine-wide profile under load): passed alone once;
  - `budget::the_extra_cpu_a_faster_poll_costs_is_the_extra_cpu_the_shell_observes`
    (tally #14): passed alone once;
  - `omn-mcp` `protocol::the_sweep_is_the_one_the_binary_that_runs_gives`
    (56 usable against 92): the `/tmp` binary carries no capabilities, an
    artifact of where it was built;
  - `vertical::without_the_capabilities_the_session_says_which_it_needs`:
    *"a copy of the binary: No space left on device"*.
  In that run the eBPF-dependent integration tests took the unprivileged path.

## CI expectation, per test this stream owns that failed at `37f17c8`

| Test | defaults (paranoid 2, no caps, delegated cgroup) | stock (paranoid 4) |
|---|---|---|
| `collect::mem::swap::tests::a_page_of_zeros_is_swapped_without_being_written` | **Unknown.** The cause on the runner was not measured, and the runner's THP mode is not in its log; on `madvise` the small-page change does nothing. If it is red again, the message carries `memory.current` and the vmstat deltas that say where the pages went | green, as at `37f17c8`: no cgroup can be made, so it skips with the reason |
| `collect::mem::pressure::tests::a_cgroup_forced_to_reclaim_stalls_and_an_idle_one_does_not` | green, reasoned: `memory.high` never calls the OOM killer, a throttled child is the stall measured rather than a timeout, and 192 MiB needs a third of the swap. A child killed some other way still panics, with its `memory.events` | green, skips as before (no delegated cgroup) |
| `scenario_mem::every_mode_and_emit_of_the_counter_sources_is_served_or_refused_by_the_right_code` | green: the user-mode retry now runs for a launched program (the library test showed the user-mode open works on that cell), so the ten combinations exit 0. Residual risk: several tests in one binary sampling at once share the user's `perf_event_mlock_kb` budget, which paranoid -1 here never charges; a ring refused there is a `ring_budget_refusal`, exit 1, and would name the limit | green, as at `37f17c8`: the resolver withholds `pmu:sw` at paranoid 4, which the sweep does not ask about |
| `scenario_net::the_trace_leg_of_the_sweep_answers_or_names_the_backend_it_would_need` | green: `/dev/ptp1` does not open there, so the exit-1 branch, whose text is the one the CI printed (`so_timestamping: phc: missing`) | green, the same |
| `scenario_mem::the_tool_finds_the_faults_the_program_counted_itself`, `the_faults_a_worker_thread_takes_are_traced_to_that_thread` (skipped at `37f17c8`) | run for the first time, user mode only: the first against `ru_minflt` within 15 %, the second against its 16 384 stores; no new skip line | skip, as before |
| `scenario_mem::both_huge_page_backends_read_the_same_promotion_counter` (green at `37f17c8`, D15 changes it) | green: both backends now read the PMD-mapped count, the bound is the measured drift | green, the same files |
| `scenario_mem::a_gibibyte_of_reads_costs_one_l1_miss_per_cache_line` | skips at the L1 request, as before (no hardware PMU) | skips, as before (paranoid 4) |

## Open

- **The zero-page swap test on the CI**, above: the cause of 22 MiB on swap is
  not measured; the next run's message will say.
- **The user-mode bound of `the_tool_finds_the_faults_the_program_counted_itself`**
  is reasoned from one `perf stat` of the workload (1 % kernel-mode faults),
  not run at paranoid 2 here.
- **The disk.** `/home` was full while this stream ran (200 G, 0 to 600 M
  free, the main checkout's `target/` alone is 25 G): `make check` in the
  worktree failed twice at link time with *"No space left on device"*, and the
  suite was run with `CARGO_TARGET_DIR` on `/tmp` instead, where the binary
  cannot carry capabilities (`/tmp` is `nosuid`, and the sudoers rule names the
  checkout's path). So the suite's eBPF-dependent tests skipped in that run;
  this stream's own test files were run with capabilities in the worktree
  before the disk filled (`scenario_mem`, `scenario_net`, `family_mem`, the
  flaky reruns).
- To commit at all, this stream removed its own worktree's `target/` (3.3 G;
  `git commit` could not write an object). The branch is committed; a `make`
  in the worktree rebuilds it once the disk has room.
- `cross-memory-which-allocation.yml` on `defaults` will run for the first
  time, user mode only; filed for its owner.

## Needs privilege

None beyond `make`. End to end for item 2 is `STATE.md`'s existing `nosuid`
item (2), unchanged:

```console
sudo mkdir -p /mnt/omn-nosuid
sudo mount -t tmpfs -o nosuid tmpfs /mnt/omn-nosuid
cp target/debug/omn /mnt/omn-nosuid/omn
sudo setcap cap_bpf,cap_perfmon+ep /mnt/omn-nosuid/omn
/mnt/omn-nosuid/omn sched.offcpu -e summary -- sleep 0.1   # the refusal says nosuid, not setcap
sudo umount /mnt/omn-nosuid
```

## For DECISIONS.md

- **A forced error path is the real one.** A test that forces a fallback
  (here, paranoid 2's user-mode retry) forces it with an error in the shape the
  layer below returns, not a hand-built one: the hand-built errno is what let a
  retry that never ran pass on the cell it was written for.
- **A cgroup test that wants reclaim uses `memory.high`**, not `memory.max`,
  unless what it measures is the limit itself: `max` adds an OOM kill that is
  the disk's speed, not the subject. And it does not require the throttled
  child to finish: the throttling is the stall.
- **Two backends of one source answer one kernel counter under one attribute
  name, or carry different names.** Matching on the day is not evidence (D6
  matched, D15 did not); where one file has no equivalent, the backend reads
  the counter where the kernel keeps it and publishes its own under its own
  name (`huge_folios`).
- **A run that exited 0 was served.** A scenario's skip test looks for a
  machine's limit in the refusal, never in the caveats of a run that worked.
- **Unbuilt and unservable here** (the `net.hwts` case): the chain path says
  the machine first, the forced path says the build first. This stream's test
  accepts both by the machine and does not choose; filed to the resolver.

## Filed for other streams

`docs/open-findings.md`, "Needs from another stream — the sixteenth wave,
stream mem":

- `.github/expected-skips.txt`: the pattern `samples page faults in user mode
  only`;
- `tests/cross_memory.rs`'s owner: `cross-memory-which-allocation.yml` meets a
  user-mode-only run on `defaults`;
- `src/resolve.rs`: one order for "unbuilt" and "missing" on both paths.

Answered without a change: the phase 6 ladder (`catalog/*.yaml`, the
fifteenth wave) waits on a decision about one series made of two modes, which
is the resolver's to file; nothing in this stream's entries was asked of yet.

## Session index rows

None: no session was added under `examples/`.
