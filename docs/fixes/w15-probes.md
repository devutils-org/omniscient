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
   triple in today's catalog), and on the binary
   `catalog_model::a_target_and_mode_no_backend_declares_together_is_refused_as_the_request`:
   the pairs found from each backend's declared targets and modes, each run
   through `omn trace`, held to exit 2 and the pair's reason.
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
5. **Phase 2 and phase 7 audits**, over the 22 cells of the finished
   2026-09-30 sweep, copied from the main checkout into this worktree's
   `target/matrix/cells`:
   `portability::two_cells_list_different_sources_for_the_condition_between_them`
   and `every_gap_between_two_cells_is_explained_by_their_resolved_records`,
   both skipping loudly without cells, and
   `a_gap_is_explained_only_by_a_recorded_field_that_differs` and
   `a_gap_is_credited_to_the_cause_the_refusal_names_not_to_any_field_that_differs`
   judging the judge. A gap is credited to the cause the refusing probe
   names in its own words (a BTF it could not read, a capability, the
   sysctl, a tracepoint the kernel does not define, hardware not published),
   and only when the field standing for it differs between the two records
   the way that refuses (a `kernel>=` requirement: the refusing side's
   release older, the other's not); a reason naming nothing a record carries
   fails. What the second found is a gap in the record itself: nothing in the
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

- **2754 gaps between the cells on disk are counted as unrecorded**, not
  explained: 2514 only the privileges could explain, 197 only the
  virtualization, 43 both, because the cells were swept by `main`'s binary,
  before the record carried them. Explained: 1716 by `kernel_btf`, 991 by
  `tracefs`, 255 by `kernel_release`, 200 by `gpus` (NVML, here against the
  guests), 20 by `perf_event_paranoid`. Measured by `every_gap_...` on the
  22 cells; the next sweep with this branch's binary turns the unrecorded
  ones into explained or failed.
- **83 differences are the build's**, between cells recorded before
  `cell.build` existed (`here-nocaps` is from 2026-09-29).
- **`portability::every_cell_on_disk_holds_the_invariants` is red on the
  cells on disk, 6 violations, none this branch's**: five in `here-nocaps`,
  the stale 2026-09-29 cell `docs/portability.md` already names (*"usable
  through `pmu`, which has no collector in this build"* for `cpu.profile`,
  and four like it), which the 2026-09-30 sweep did not redo; and one in
  `here-caps`, `net.hwts --system -m trace` listed usable through
  `netlink:ethtool` and refused at the start (exit 3), filed for the net
  family's owner. It was 60 in review because the cells had been copied
  mid-sweep: `7.2.6-nobtf` and `-notracefs` were still the previous build's
  and `7.2.6-nocaps` had no `sweep.json` yet.
- `cpu.cycles` through `pmu:sw` reports its value in the entry's unit
  (`count`): the degradation line says "CPU time, not cycles", and the
  counter's name is `cpu_clock_ns`, but a per-backend unit would need
  `Backend.unit` in `src/catalog.rs` (R's), not filed as it changes the
  catalog's model.
- The attach-time privilege advice (`bpf::permission_advice`) still says
  `setcap` for a withheld capability (filed; the probe's is rewritten).

## Review fixes

1. **The resolver test this branch broke, in a file it does not own**:
   `resolve::tests::a_source_viable_only_through_a_backend_nobody_wrote_is_not_listed_usable`
   is replaced in a commit of its own that touches `src/resolve.rs` alone,
   so the branch merges green and R can drop the commit for another fix.
   The unwritten backend is `cpu.cycles`'s `pmu:sw` declaration renamed to
   `fs:proc`, asserted unwritten; finding the pair at run time was tried
   and found none on this build, a permanent skip. The request to R says so.
2. **The phase 7 test credited any differing field**: rewritten (item 5
   above). The reviewer's two cases are the new unit test's first two: a
   `bpf` gap across kernels is the privileges' (unrecorded), not
   `kernel_release`'s; a `hwmon` gap is the virtualization's (unrecorded),
   not `gpus`'. On the cells, `kernel_release` went from 1740 to 255 and
   `gpus` from 296 to 200 (all NVML), and the unrecorded count from 473 to
   2754.
3. **`every_cell_on_disk_holds_the_invariants` red**: real, and now said
   under Open with its six violations on the finished sweep; not this
   branch's, and not made green by weakening the judge.
4. **No integration test held the pair refusal's exit code**: added
   (item 2 above). The `nosuid` diagnosis end to end stays under Needs
   privilege.
5. **Found by the check**: the user-mode page-fault test's bound counted
   samples against pages (see below); it counts pages now.

## This stream's check

After the review fixes, one `make check`: red in the library at
`collect::mem::page_faults::tests::sampled_in_user_mode_the_programs_own_faults_arrive_and_the_kernels_do_not`
(*"65 user-mode samples landed in the 64 pages this code touched"*), which
passed alone at once. It is this stream's file, and the bound was a count of
samples held to a count of pages: a retried fault raises the event twice.
`landed_in` now counts the pages that took a fault (both page-fault tests
use it). Then `cargo test --workspace --no-fail-fast`, with the capabilities
on the binary: 2085 passed, 3 failed, 8 ignored. The three:
`portability::every_cell_on_disk_holds_the_invariants` (the six stale or
filed violations under Open), and `runner::a_source_attached_for_one_phase_counts_that_phase_and_nothing_of_the_other`
and `unwind::each_mode_copies_what_it_declares_and_dwarf_costs_more_per_sample_than_fp`,
neither this branch's, both passing alone at once (for the flaky tally).
`resolve::tests::a_source_viable_only_through_a_backend_nobody_wrote_is_not_listed_usable`
passes. Clippy (`--workspace --all-targets -D warnings`) and fmt clean.

Before the review, `make check` had stopped at that resolver test, and the
workspace run had 2203 passed and ten unrelated failures, each passing alone
at once except `scenario_mem::both_huge_page_backends_read_the_same_promotion_counter`
(for its owner and the flaky tally).

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
  `pmu:sw` as unwritten -- replaced on this branch in its own commit; R keeps
  it or drops it.
- The net family's owner: `net.hwts`'s `netlink:ethtool` declares
  `modes: [snapshot, poll]`.
- The owner of `src/bpf/mod.rs`: `permission_advice` calling
  `capabilities::withheld_file_capabilities()`.
- The runner stream (`examples/bench/harness.rs`): the guardrail through
  `power.throttle`, or the phase 9 sentence corrected.
- R (`src/catalog.rs`): `Cost::Extreme`'s doc comment (J's, again).
- The coordinator: `CATALOG.md:510`, a `docs/scenarios/cpu.md` row,
  `CLAUDE.md`'s counts (92 and 56 here), `DECISIONS.md`'s open line.

## Session index rows

None: no session file was added under `examples/`.
