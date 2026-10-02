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
- `the_tool_finds_the_faults_the_program_counted_itself`: at least 128 (256 MiB
  in PMD-sized folios at most) and at most `ru_minflt`, then a loud *"skipped
  the exact comparison: this machine samples page faults in user mode only"*.

### `collect::mem::pressure::tests::a_cgroup_forced_to_reclaim_stalls_and_an_idle_one_does_not` (defaults): the test's

`memory.max` at 24 MiB with 512 MiB touched had the child OOM-killed on the
CI's swap file (`memory.events`: `max 1631 oom 1 oom_kill 1`). The cell is the
first to run this test at all: only `defaults` delegates a cgroup. **Fix**:
`memory.high` instead. Both make the kernel reclaim from the cgroup as it
grows, and the waiting is the stall measured; only `max` calls the OOM killer
when reclaim falls behind, and over `high` a task that outruns reclaim is
throttled, which is itself counted as memory stall. Passed three times in a
row here, alone.

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

## 6. The flaky tally entries, alone, five times each

35 of 35 passed, at load average 2 to 5: `scenario_mem::both_huge_page_backends_read_the_same_promotion_counter`
(#21), `a_gibibyte_of_reads_costs_one_l1_miss_per_cache_line`,
`a_walk_that_fits_in_l1_is_never_timed_and_one_that_does_not_waits_on_memory`,
`family_mem::the_working_set_is_what_was_touched_and_not_what_was_held`,
`scenario_proc::defect_1_...`, `defect_2_...`,
`scenario_sched::a_forced_switch_program_splits_the_switches_as_the_kernel_counts_them`.
None failed again, so no ground truth changed; `docs/flaky.md` has the table.

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
| `collect::mem::swap::tests::a_page_of_zeros_is_swapped_without_being_written` | **Expected green, not proven**: small pages remove the one mechanism found that takes zero pages out without a swap entry. If it is red again, the message now carries the vmstat deltas that say where the pages went | green, as at `37f17c8`: no cgroup can be made, so it skips with the reason |
| `collect::mem::pressure::tests::a_cgroup_forced_to_reclaim_stalls_and_an_idle_one_does_not` | green: `memory.high` never calls the OOM killer, and the stall is measured either way | green, skips as before (no delegated cgroup) |
| `scenario_mem::every_mode_and_emit_of_the_counter_sources_is_served_or_refused_by_the_right_code` | green: the user-mode retry now runs for a launched program (the library test showed the user-mode open works on that cell), so the ten combinations exit 0. Residual risk: several tests in one binary sampling at once share the user's `perf_event_mlock_kb` budget, which paranoid -1 here never charges; a ring refused there is a `ring_budget_refusal`, exit 1, and would name the limit | green, as at `37f17c8`: the resolver withholds `pmu:sw` at paranoid 4, which the sweep does not ask about |
| `scenario_net::the_trace_leg_of_the_sweep_answers_or_names_the_backend_it_would_need` | green: `/dev/ptp1` does not open there, so the exit-1 branch, whose text is the one the CI printed (`so_timestamping: phc: missing`) | green, the same |
| `scenario_mem::the_tool_finds_the_faults_the_program_counted_itself`, `the_faults_a_worker_thread_takes_are_traced_to_that_thread` (skipped at `37f17c8`) | run for the first time, user mode only, against the floors above; one new skip line, filed for the gate | skip, as before |

## Open

- **The zero-page swap test on the CI**, above: the cause of 22 MiB on swap is
  not measured; the next run's message will say.
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
  the disk's speed, not the subject.
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
