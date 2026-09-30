# The fifteenth wave, stream probes

What the probes, the validator, the `cpu` counters and the matrix judges
changed, each with the test that holds it. `docs/portability.md`, "The
fifteenth wave", has the same items with the measurements; the requests to
other streams are in `docs/open-findings.md`, "Needs from another stream --
the fifteenth wave, stream probes".

## Landed

1. **`container: no` dropped where the cgroup is served.** The eleven filed
   entries (`cpu.cycles`, `.instructions`, `.ipc`, `.branch_misses`,
   `.cache_misses`, `.topdown`, `.context_switches`, `.migrations`,
   `.throttle`, `cpu.profile`, `mem.faults`), the three `mem.cache_*` entries
   read through the same counter group, and `sched.switches` with its two
   programs. The validator's *"which this tool does not do"* arm is gone, and
   the loader refuses a catalog that says `container: no` beside a served
   cgroup, entry or backend (`validate::container_complaints`). The two
   collectors' unbound-container refusals now say what a library caller
   skipped (`request::find_container`).
   - `scenario_cpu::a_container_is_counted_as_the_cgroup_its_runtime_names`
     (ground truth: the child's own `retire` loop inside, ten times it
     beside; the event keyed on the cgroup directory's inode).
   - `scenario_cpu::a_container_is_asked_of_its_runtime_where_its_cgroup_is_served`
     replaces `a_container_is_refused_before_the_start_where_only_its_cgroup_is_served`
     in the same commit (exit 1, the runtime's own answer).
   - `validate::tests::a_container_is_accepted_wherever_its_cgroup_is_served`.
   - R's `tests/cli.rs::a_container_is_served_by_a_source_that_serves_its_cgroup`
     now runs: 388 switches in the container's cgroup, 385 blocks by the
     kernel's count.
2. **A (target, mode) pair no backend declares is exit 2, with the reason**
   (`validate::check_pair`): `sched.switches --cgroup -m trace` names each
   backend's half. `validate::tests::a_target_and_mode_no_backend_declares_together_are_refused_as_the_request`
   and `a_pair_refused_as_the_catalogs_would_not_have_resolved_on_this_machine`
   (every refused triple held to the resolver's walk on this machine; one
   triple in today's catalog).
   **A binary whose file capabilities the kernel withheld is told why**:
   `capabilities::withheld_file_capabilities()` reads `security.capability`
   off `/proc/self/exe` and `CapEff`, and names the `nosuid` mount (with
   `findmnt`), `no_new_privs`, or a grant without `e`; `Capabilities::probe`
   rewrites the privilege refusals with it. Tests:
   `a_mount_is_nosuid_exactly_when_statvfs_says_so` (against `statvfs`),
   `a_binary_s_file_capabilities_are_read_as_getcap_prints_them` (against
   `getcap`), `a_capability_the_file_carries_and_the_process_lacks_is_explained_by_its_mount`,
   and end to end `portability::a_binary_denied_its_file_capabilities_says_why_instead_of_advising_the_grant`
   (the real binary under `PR_SET_NO_NEW_PRIVS`).
   **The two expected-skips lines**: the cgroup-ancestry wording in, the
   `/sys/class/block/root` line out (`.github/expected-skips.txt`).
3. **`cpu.cycles` on `pmu:sw`**: the kernel's cpu clock as the degraded
   fallback the catalog declared, one row of `counters::SPECS`, counter name
   `cpu_clock_ns`. `counters::tests::the_cpu_clock_fallback_counts_the_nanoseconds_the_thread_ran`
   (the thread's `CLOCK_THREAD_CPUTIME_ID`, to a tenth) and
   `scenario_cpu::a_cycle_count_and_its_cpu_clock_fallback_differ_by_the_core_s_frequency`
   (400 ms burned counted to 15 %; `pmu` over `pmu:sw` inside cpufreq's
   range, 4.2 GHz here). `every_backend_either_measures_or_says_it_is_not_built`
   names that comparison; its `unbuilt` example is `cpu.pressure` on
   `fs:cgroup` now.
4. **Phase 9's guardrail**: stated in `docs/portability.md` (item 4) why the
   bench reads the throttle counters directly and what that is not; the call
   filed for the runner stream with a signature.
5. **Phase 2 and phase 7 audits**, over the 20 cells copied from the main
   checkout into this worktree's `target/matrix/cells`:
   `portability::two_cells_list_different_sources_for_the_condition_between_them`
   and `every_gap_between_two_cells_is_explained_by_their_resolved_records`,
   both skipping loudly without cells, and
   `a_gap_is_explained_only_by_a_recorded_field_that_differs` judging the
   judge. What the second found is a gap in the record itself: nothing in the
   resolved file said what the process was privileged to do, or whether the
   machine was a guest. `Environment.privileges` and
   `Environment.virtualization` are captured now
   (`the_privileges_recorded_are_the_ones_capget_returns`,
   `a_virtual_machine_is_recorded_as_one_where_systemd_detects_one`), and
   `not_captured` loses "virtualization". A cell records the hash of the
   binary that swept it (`cell.build`).
6. **The fourteenth wave's requests against these files**: the eleven
   entries (R, U), the validator sentence (R), the pair refusal (K), the
   `nosuid` diagnosis (D), both expected-skips lines (R, K). Already on
   `main` and checked: `check_decomposition_in` in the validator (J), the
   kernel-only switch counters in the probe and at the attach (K), `Rule::via`
   optional (J).

## Open

- **473 gaps between the cells on disk are counted as unrecorded**, not
  explained: 335 only privileges could explain and 138 privileges or
  virtualization, because the cells were swept before the record carried
  them. Measured by `every_gap_...` on the 20 cells; the next matrix sweep
  turns them into explained or failed.
- **217 differences are the build's**, between cells swept by two binaries
  (2026-09-29 and 2026-09-30) and recorded before `cell.build` existed.
- `cpu.cycles` through `pmu:sw` reports its value in the entry's unit
  (`count`): the degradation line says "CPU time, not cycles", and the
  counter's name is `cpu_clock_ns`, but a per-backend unit would need
  `Backend.unit` in `src/catalog.rs` (R's), not filed as it changes the
  catalog's model.
- `resolve::tests::a_source_viable_only_through_a_backend_nobody_wrote_is_not_listed_usable`
  fails by design on this branch (filed for R, with the replacement).
- The attach-time privilege advice (`bpf::permission_advice`) still says
  `setcap` for a withheld capability (filed; the probe's is rewritten).

## This stream's check

`make check` stopped at the library's one expected failure
(`resolve::tests::a_source_viable_only_through_a_backend_nobody_wrote_is_not_listed_usable`,
filed). `cargo test --workspace --no-fail-fast` then, with five other streams
and a matrix sweep on the machine: 2203 passed, that one failed, and ten more
failed that this branch does not touch -- `budget`, `cross_compute`, `cross_io`,
`encoders`, `family_sched` (0 migrations counted for 58 moves), `scenario_net`
(`net.hwts` forced exited 1), `triage`, two `unwind` -- each of which passed
alone at once. One failed again alone:
`scenario_mem::both_huge_page_backends_read_the_same_promotion_counter`
(*"`fs:sys` reports 570 fully-mapped huge pages and /proc/meminfo says between
489 and 508"*), whose entry and collector this branch does not change: for the
flaky tally, and its owner. Clippy and fmt clean.

## Needs privilege

- **A `nosuid` mount, end to end.** Unprivileged, a copy cannot keep
  `security.capability` (it needs `CAP_SETFCAP`). A human runs:

  ```console
  sudo mount -t tmpfs -o nosuid tmpfs /mnt/omn-nosuid
  cp target/debug/omn /mnt/omn-nosuid/omn
  sudo setcap cap_bpf,cap_perfmon+ep /mnt/omn-nosuid/omn
  /mnt/omn-nosuid/omn list sched.offcpu | grep 'needs    bpf'
  # expected: "... The binary /mnt/omn-nosuid/omn carries CAP_BPF and CAP_PERFMON,
  #            but this process was not given them: the filesystem at /mnt/omn-nosuid
  #            is mounted nosuid ..."
  sudo umount /mnt/omn-nosuid
  ```

- **The matrix sweep**, alone at the end of the wave, to turn the unrecorded
  gaps into explained ones: `scripts/matrix.sh here && scripts/matrix.sh &&
  cargo test --test portability` (it boots guests; never run by this stream).

## For DECISIONS.md

- **`cpu.cycles` on a cpu with no hardware PMU is served by the cpu clock**
  (`pmu:sw`, degraded, "CPU time, not cycles: blind to DVFS"), closing the
  open line. Every mechanism as a chain, with a fallback that says what it
  degrades; the cost is a number in the entry's unit that is nanoseconds, said
  on the `degraded:` line and in the counter's name.
- **A container is its cgroup in the catalog's model, with no exception**:
  `container: no` beside a served cgroup is a catalog that does not load.
- **A (target, mode) pair no backend of the chain declares is the request's**
  (exit 2, before resolution), the way a mode the entry does not declare is.
- **A privilege refusal where the file carries the capability names why the
  kernel withheld it**, and never advises the grant already made.
- **The resolved record carries the privileges and the virtualization**, so a
  gap between two machines is explained from their two records; a matrix cell
  carries the hash of the binary that swept it.

## Filed for other streams

In `docs/open-findings.md`, "Needs from another stream -- the fifteenth wave,
stream probes":

- R (`src/resolve.rs`): the resolver test that named `cpu.cycles` on
  `pmu:sw` as unwritten, with its replacement.
- The owner of `src/bpf/mod.rs`: `permission_advice` calling
  `capabilities::withheld_file_capabilities()`.
- The runner stream (`examples/bench/harness.rs`): the guardrail through
  `power.throttle`, or the phase 9 sentence corrected.
- R (`src/catalog.rs`): `Cost::Extreme`'s doc comment (J's, again).
- The coordinator: `CATALOG.md:510`, a `docs/scenarios/cpu.md` row,
  `CLAUDE.md`'s counts (92 and 56 here), `DECISIONS.md`'s open line.

## Session index rows

None: no session file was added under `examples/`.
