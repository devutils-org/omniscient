# The sixteenth wave, stream cpu

What the `cpu` family, the unwinder and the profile changed, each with the
test that holds it. The requests to other streams are in
`docs/open-findings.md`, "Needs from another stream -- the sixteenth wave,
stream cpu". Branch `feat/w16-cpu`, from `37f17c8`.

## Landed

1. **A forced `--stack` mode this machine cannot serve is exit 1**, the
   machine's gap (`unwind::forced`, `Whose::Machine`), and exit 3 stays for a
   mode the machine would serve and no collector reads ("not implemented").
   Resolver's filing of the fifteenth wave, in one commit with its test:
   `scenario_cpu::an_unwinder_this_build_lacks_is_refused_before_the_program_starts`
   (here: `lbr` and `sframe` both exit 1, the program's marker file absent).
2. **The CI's reds in this stream's files** (one commit, `9ea6286`), each
   now holding the tool to what the kernel or the workload says:
   - `unwind::each_mode_copies_what_it_declares_and_dwarf_costs_more_per_sample_than_fp`.
     *The copy* (the defaults cell's red: 0 of 432 dwarf samples carried
     min(8192, top - sp), each about 2600 bytes): the kernel copies a user
     stack where no page fault may be taken (`copy_from_user_nmi`), so it
     stops at the first non-resident page above sp. `exec` leaves one there
     at random -- the gap `arch_align_stack` puts below the strings at the
     top, up to 8 KiB, which nothing writes. Measured here: a non-resident
     page among the top four of `[stack]` in three of five runs of the
     fixture. The test reads the target's `/proc/<pid>/pagemap` while it
     runs and expects min(declared, first non-resident page above sp, top).
     Not a tool defect: the frames between sp and `main` are always
     resident, so what is cut is the argument vectors. *The cost* (the
     local flaky tally, twice in a row at load 12 to 18): what a sample
     costs in cpu time moved by a factor of two between two `fp` runs a
     minute apart, the size of the separation. fp and dwarf now run side by
     side, three times, compared pooled. Under 14 busy loops beside the
     other streams (load 12.6), three runs in a row: fp 18.6 to 25.1 us a
     sample, dwarf 30.5 to 61.1, separating each time.
   - `unwind::a_stripped_build_is_named_from_debuginfod_only_when_asked_and_as_the_unstripped_one_is`
     (defaults: one `["spin"]` stack in one run of the two). A walk cut
     short of `main` is held to be the leaf end of a whole path and under
     5 % of samples, rather than failing the set of whole paths, which is
     still held equal. Every stack's count is in the failure message now,
     because a second shape was seen once in this stream's own workspace
     run at load 12 -- the unstripped reference missing the whole `compare`
     branch (about 95 samples of 300) -- and not reproduced in 18 runs
     since, six of them under 14 busy loops. Open, below.
   - `unwind::a_mode_this_machine_cannot_serve_is_refused_with_the_requirement_it_lacks`
     (stock): `cpu.profile` itself has no viable backend at paranoid 4, so
     the stack mode is never asked; skips with the probes' reasons first.
   - `unwind::the_choice_says_user_only_exactly_when_the_machine_refuses_the_kernel`
     (stock): skips at paranoid 3 and above without the capabilities,
     where no choice of halves exists ("perf_event_paranoid is 4, which
     refuses an unprivileged process every perf event, ...").
   - `stack::tests::a_forced_mode_is_served_as_asked_or_refused_with_the_reason`
     and `auto_chooses_a_concrete_mode_and_says_why_it_passed_over_the_others`
     (stock): `sampling_refused_here()` reads the paranoia and `CapEff`;
     at 3 and above the forced test still holds every mode's refusal to
     "`--stack <mode>`" and "missing", then skips with the reason.
   - `scenario_cpu::a_profile_samples_every_thread_for_the_cpu_it_used`
     (defaults: 39 samples for 200 ms of a thread's cpu). `burn` read
     `CLOCK_THREAD_CPUTIME_ID` every turn, which on x86-64 is a system call
     (no vDSO for a thread's clock), so most of the burn was in the kernel,
     which a user-only profile at paranoid 2 does not sample -- and the
     tool said exactly that in its degradations. The fixture now burns in
     user mode, 20 000 turns of arithmetic between clock reads. Every
     `burn:` workload of the file gains the same.
   - `scenario_cpu::a_profile_driven_past_the_kernels_ceiling_says_what_it_did_not_take`
     (defaults: not throttled). `cpu.profile` is served by `pmu:sw` only
     (the `pmu` collector is not written), the cpu clock, an hrtimer the
     kernel never arms for less than 10 us -- which at the default ceiling
     of 100 000 samples a second is the ceiling itself. It passes the
     ceiling only by the timer's jitter around the tick, which this machine
     has and the CI's VM did not. When not throttled, served by `pmu:sw`
     and the ceiling is at least 100 000, it skips with the reason: "this
     cpu offers no hardware PMU to sample on" when a hardware cycles
     sampler cannot be opened (asked of the kernel directly), and "this
     build samples through the cpu clock only" otherwise.
   - `collect::cpu::oncpu::tests::a_program_that_burns_cpu_is_reported_as_having_burnt_it_and_one_that_sleeps_is_not`
     (defaults: a life of 2140 ms reported as 780 ms). The busy program's
     cpu is the scheduler's `schedstat` for it, held to 0.6 to 1.5 of what
     is reported (as before), and at least 100 ms; its wall clock, which the
     CI's load made 2.7 times its cpu, is no longer a reference. The
     sleeper's half is unchanged.
3. **A user-only profile does not read the kernel's symbol table**
   (`ProfileCollector::start` primes `/proc/kallsyms` only when the
   callchain has a kernel half; it was 978 ms of a debug build's cpu before
   a one-second window at paranoid 2). Held by
   `collect::cpu::profile::tests::a_profile_reads_the_kernels_symbols_only_when_its_stacks_have_a_kernel_half`:
   the bytes the starting thread read (`/proc/thread-self/io`, `rchar`)
   against the size of `/proc/kallsyms`, in a fresh copy of the test binary,
   user-only first. Here: 98 bytes for a user-only start, 19 011 813 for one
   with the kernel, against a 19 011 713-byte table; with the old condition
   the user-only start read 19 011 811 and the test failed. Held in bytes
   read rather than in `cost.json`'s time before the window, because the
   read is the cause and a time is a load-dependent proxy of it.
   **The factory takes the resolver's choice** (`ResolvedRequest.stack`,
   optional item): `cpu.profile`'s `pmu:sw` factory uses it when present and
   calls `stack::choose` only for a request resolved without one.
4. **`Backend.unit`**: a backend's own unit when it is not the entry's,
   read through `Entry::backend_unit`. The loader refuses one that repeats
   the entry's, one on the head of the chain, one on an `equivalent`
   backend, and one that does not add up where the entry's does
   (`check_backend_units`). `cpu.cycles`'s `pmu:sw` declares `unit: ns`.
   `catalog::tests::a_backend_reports_in_its_own_unit_only_where_it_is_a_fallback_that_says_so`.
   The result's unit is chosen in `src/main.rs` and `src/triage/engine.rs`
   (not this stream's), so the output still says `count` until the filed
   one-line change lands;
   `scenario_cpu::the_cpu_clock_fallback_of_cpu_cycles_reports_nanoseconds_and_the_counter_a_count`
   holds it (the workload's 300 ms of its own cpu, the JSON `unit`, a time
   in the text) and is `#[ignore]`d until then. Run with the `main.rs` line
   applied and `--ignored`, it passed here (`303.19ms` in the text, `ns`
   and `count`). `docs/scenarios/cpu.md`'s "Do two roads to the same number
   agree?" is rewritten for the chain with two collectors, with the row
   probes asked for.
5. **Four continuations that lost their `\`**: `src/perf.rs:3457`,
   `src/collect/cpu/counters.rs` (two; the filing's 1354 and 1401 had moved
   to 1410 and 1457), `tests/scenario_runtime.rs:240`. `grep -rnE
   '"[^"]*\S {10,}\S[^"]*"'` finds none in this stream's files.
6. **Skip lines that name the machine's reason** (not asked; read off the
   skip gate of both cells, which failed partly on this stream's lines):
   `tests/unwind.rs`'s four perf refusals say "this machine will not sample
   this process (perf_event_paranoid is N)"; the per-ring test asks for the
   kernel half only where `kernel_callchain_allowed()` (it skipped at
   paranoid 2 for no reason, its subject being the rings);
   `scenario_cpu`'s skips that quote stderr put the reason lines first and
   indent the rest (`machine_s_reason`, as stream engine's in wave 15),
   which is what made `note: container ...` the reason on defaults; the
   throttle scenario's cgroup skip says "no cgroup here delegates the cpu
   controller to this user"; two counter tests name the paranoia.

## Checks

- While iterating: `cargo test --lib` for `stack::`, `collect::cpu::`,
  `catalog::`, `perf::`, and `--test scenario_cpu`, `unwind`, `family_cpu`,
  `stack_capture`, `scenario_runtime`: all green with the capabilities on
  the binary (one `scenario_cpu` red was the machine's: the copy of the
  binary hit "Disk quota exceeded" in `/tmp`, and passed alone at once).
- `make check` once, on the tree of the first five commits: red at
  `budget::the_extra_cpu_a_faster_poll_costs_is_the_extra_cpu_the_shell_observes`
  (flaky tally #14, not this stream's; failed again alone, passed in the
  workspace run). Then `cargo test --workspace --no-fail-fast` once, load
  up to 12 on 24 cpus and `/home` and `/tmp` full: red only in
  `scenario_io` (17) and `scenario_net` (8), every one `ENOSPC` writing a
  file; `recorder::two_bursts_...`, `scenario_mem::a_cgroup_squeezed_...`
  and two `scenario_power` tests, all passing alone at once; and
  `unwind::a_stripped_build_...` (the shape under Open). Clippy
  (`--all-targets -D warnings`) and fmt clean.
- `make check` once more, on the final tree (item 6 added): red in the
  library at `collect::mem::page_faults::tests::faults_are_reported_at_the_address_that_faulted`
  (*"65 events landed in a block of 64 pages"*, stream mem's file, in no
  tally; passed alone at once -- the shape probes fixed in its other test in
  wave 15, a retried fault raising the event twice). Then `cargo test
  --workspace --no-fail-fast` once, at load average 94 to 126 on 24 cpus and
  `/home` at 100 %: **2122 passed, 7 failed, 11 ignored**, none of the seven
  in this stream's files: five `scenario_io` (the disk),
  `scenario_net::every_datagram_sent_to_a_closed_port_is_a_drop_with_one_reason`
  (machine-wide drops: 2003 seen, none `NO_SOCKET`, failed alone too),
  `walkthrough::the_walk_through_hands_over_...` (blocked 200.0 ms against
  the program's 202.7). Clippy and fmt clean, `target/debug/omn` carries
  `cap_bpf,cap_perfmon` (`make`).
- **The disk**: `/home` was at 100 % twice during this stream (the wave-15
  worktrees hold about 7.5 GB each), and one write truncated
  `tests/scenario_cpu.rs` to zero bytes; it was restored from `HEAD` and the
  edits reapplied. This stream removed its own `target/debug/incremental`
  (1.5 GB) and its tests' stale scratch directories, nothing else.

## CI expectation, per test this stream owns that the last push failed

| Test | defaults (paranoid 2, no PMU) | stock (paranoid 4) |
|---|---|---|
| `unwind::a_mode_this_machine_cannot_serve_...` | passed there; unchanged path (refused naming the requirement) | skip: the probes' reasons first (`perf_event_paranoid is 4`, `has no viable backend`) |
| `unwind::the_choice_says_user_only_...` | passed there; unchanged | skip: "perf_event_paranoid is 4, which refuses ..." |
| `unwind::each_mode_copies_...` | green: the copy is held to the pagemap, which is what made it red; the cost half passed there alternated and now runs side by side on 4 vcpus | skip: "will not sample this process (perf_event_paranoid is 4)" |
| `unwind::a_stripped_build_...` | green: the `["spin"]` it failed on is the leaf end of every whole path | skip, as above |
| `stack::tests::a_forced_mode_...` | passed there; unchanged | skip after holding every refusal: "perf_event_paranoid is 4, which refuses every perf event ..." |
| `stack::tests::auto_chooses_...` | passed there; unchanged | skip, same reason |
| `scenario_cpu::a_profile_driven_past_the_kernels_ceiling_...` | skip: "this cpu offers no hardware PMU to sample on, ..." (no hardware event is published on the VM, so the direct open fails) | passed there (skipped by "will not sample at all", the run refused) |
| `scenario_cpu::a_profile_samples_every_thread_...` | green: the threads burn in user mode, so the user-only samples cover them (expected 199 a thread, within 30 %) | passed there (refused, skipped) |
| `collect::cpu::oncpu::tests::a_program_that_burns_cpu_...` | green: held to `schedstat`, which the load does not inflate | passed there |
| `scenario_cpu::an_unwinder_this_build_lacks_...` | green: exit 1 for "cannot be served here", or the source withheld and skipped | skip (withheld) |

What is not reproduced here and so is reasoning only: the CI's paranoia (this
machine is at -1 with the capabilities), its missing PMU, and its timer. The
skip texts are written against `.github/expected-skips*.txt` as they stand.

## Open, with the measured reason

- `unwind::a_stripped_build_...`, the second shape: the unstripped
  reference missing the whole `compare` branch once, in a workspace run at
  load 12 with `/tmp` and `/home` full. Not reproduced in 18 runs; the
  failure message now carries every stack with its count, for the next one.
  For the flaky tally.
- `unwind::each_mode_copies_...`'s last half: `cost.json`'s
  `collector_cpu_ns` is 38 ms (fp) and 71 ms (dwarf) where the kernel
  counts 1.7 s of `omn` beyond the target (fourteenth wave's finding,
  unchanged: the symbolizing after the target exits).
- `Backend.unit` reaches the output only with the filed lines in
  `src/main.rs` and `src/triage/engine.rs`; until then
  `cpu.cycles --backend force:pmu:sw` still says `count`.

## Needs privilege

Nothing. Everything here ran with what `make` grants. **For the user, not a
privilege but a blocker**: `/home` is full (the fifteenth wave's worktrees
`omniscient-{engine,kernel,runner,probes,resolver}`, about 7.5 GB each, and
`/home/moha/repos/omniscient` at 25 GB). Removing the merged worktrees, or
their `target/`, from the main checkout:

```console
cd /home/moha/repos/omniscient
git worktree list
for w in engine kernel runner probes resolver; do rm -rf ../omniscient-$w/target; done
```

## For DECISIONS.md

- **A forced `--stack` mode whose requirement the machine lacks is the
  machine's refusal, exit 1**, and one the machine would serve with no
  collector is the build's, exit 3: the two answers `Whose` already
  defines, rather than one code for both.
- **A backend may declare its own `unit`**, only as a fallback that says
  what it degrades, never on the head or on an `equivalent` backend: a
  value in another unit is another measurement, and the result says the
  unit of the backend that served it. Chosen over renaming
  `cpu.cycles`'s unit or splitting the source, because the chain is the
  project's shape for "several mechanisms, one job" and a degraded
  fallback is allowed to measure something else as long as it says so.
- **A test's ground truth for the user stack a sample copied is the
  target's pagemap**, not the mapping's extent: the kernel's copy is
  resident memory only.

## Filed for other streams

`docs/open-findings.md`, "Needs from another stream -- the sixteenth wave,
stream cpu":

- `src/main.rs:~1337` and `src/triage/engine.rs:~214`:
  `entry.backend_unit(&serving.backend.id)` for a result's unit, then the
  `#[ignore]` off `scenario_cpu::the_cpu_clock_fallback_of_cpu_cycles_...`.
- `CATALOG.md`: the backend key `unit`.
- `src/collect/mod.rs:~2159` (`late_stacks`) and `src/collect/mem/ibs.rs:626`:
  prime the kernel's symbols only for a kernel half.

## Session index rows

None: no session file was added under `examples/`.
