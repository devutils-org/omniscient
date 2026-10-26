# The seventeenth wave — stream core

Branch `feat/w17-core`, from `main` at `8725ae0`. The files: `src/emit.rs`,
`src/resolve.rs`, `tests/cross_memory.rs`, `docs/portability.md`, `LICENSES/`,
`scripts/release.sh`, `Makefile` (install), `README.md` (licence lines).

| Item | State | Commit |
|---|---|---|
| A kernel-built histogram's `sum` from `total_ns` | landed | `e9f2fa9` |
| `stack` weighs its rows; `max` is never below the mean (review) | landed | `223542e` |
| One order for "unbuilt" and "missing", on the forced path too | **written, then reverted; re-filed** | `1aa114f`, reverted by `4c13181` |
| `cross-memory-which-allocation.yml` meets a user-mode-only run | landed | `c4985ae`, `af14f63` |
| `docs/portability.md:184`, `sched.switches` on 5.15 | landed | `77488f3` |
| The GPL-2.0 text, and the README's licence lines | landed | `1f5c1e7` |

## 1. A kernel-built histogram's `sum` (`src/emit.rs`)

Every weighted row built in the kernel carries `weight` = the count and the
exact sum as `total_ns`, and as its value its bucket's low bound: `io.latency`
(both collectors), `proc.syscall`, `sched.runqlat`, `sched.futex`, `sched.lock`,
`sched.irq`, `net.qdisc`, `net.softirq`, `mem.stall`. `sched.offcpu`'s value is
the row's mean (`total_ns / blocks`) instead; `weighed` returns `total_ns` for
it all the same. `hist`
added up `value * weight`, a lower bound up to a factor of two short, with
nothing saying so.

**Fix**: a new reserved name, `emit::SUM_NS_ATTR` (`"total_ns"`), and one
function, `weighed(event, attr, value)`, that every emit adding values up now
goes through: `hist`'s `sum`, `summary`'s `mean`, `rate`'s `total`, `topk`
(both rankings) and -- since the review round -- `stack`'s per-stack total. It takes `total_ns` when the row **declares a valid
weight** and the value attribute is a duration in nanoseconds (`*_ns`) that is
not `total_ns` itself; otherwise `value * weight` as before. The buckets are
unchanged, and `min` stays the lowest bucket bound. `max` (in `hist` and
`summary`) is the highest of the rows' low bounds and, for a row carrying
`total_ns`, of its exact mean (`highest`), so it is never below the mean beside
it; both stay lower bounds, and `SeriesHist`'s and `SeriesSummary`'s doc
comments say so.

Wider than the filing on purpose: the filing asked for `hist`, but `summary`'s
mean, `rate` and `topk` were the same `value * weight`, and leaving them would
have made a histogram's `sum / count` disagree with the same run's `summary`
mean.

**Shown**: `emit::tests::a_kernel_built_histogram_sums_what_its_rows_measured_not_their_low_bounds`
(two rows, 300 and 20 reads; the sum, mean, rate total and ranking are the
exact 7 000 000 ns, not the low bounds' 5 570 560; `bytes` on the same rows and
a row with no weight are weighed as before). The full suite is green over it.
**Reasoned, not shown**: the end-to-end number for `io.latency --emit hist` in
the kernel against the stream (the filing's 6 512 640 vs 7 913 727 ns) --
`scenario_io` is stream io's file, and it bounds the two histograms against
each other already; whether it now wants an assertion on `sum` is io's call.

## 2. One order for "unbuilt" and "missing" (`src/resolve.rs`): re-filed

`1aa114f` does what the brief decided: the pinned path refuses a backend for
its missing requirements before asking whether it is built (as the walk does),
and `key_elsewhere` (the "which backend attaches this key" hint) asks the
machine first too, so the hint, the walk and the pin give one order. Its own
tests (`resolve::tests`) were rewritten and pass.

**What blocked it**: eleven tests outside this stream's files pin a backend
nothing collects in order to reach the build's refusal (exit 3), and on a
machine where that backend also lacks something -- here, tracefs is mounted
but not readable by uid 1000, so every `bpf:tp` pin probes missing -- the pin
now answers the machine's (exit 1). The full run over `1aa114f`
(`cargo test --workspace --no-fail-fast`) failed exactly these, plus two
unrelated ones that passed alone:

| Test | What it asserted | What it becomes |
|---|---|---|
| `src/collect/mod.rs` `collect::tests::a_filter_the_backend_pushed_down_is_not_run_a_second_time` | pinned `io.latency` `bpf:tp` resolves in-process | resolve against a catalog copy with the backend's `requires` cleared (patch below; shown passing) |
| `tests/scenario_proc.rs:682` `a_source_this_build_does_not_implement_exits_three_and_names_the_backend` | `proc.exec --backend force:bpf:tp` exits 3 "no machine can turn it into any other answer" | branch on the backend's probes: exit 1 naming the missing requirement where it lacks one, exit 3 otherwise; or pin a backend whose requirements hold everywhere |
| `tests/scenario_sched.rs:1641` `pinning_a_backend_this_build_lacks_blames_the_build_and_not_the_machine` | `sched.offcpu` `bpf:tp:stacks` pin exits 3 | the same branch |
| `tests/scenario_power.rs:1507` `pinning_a_backend_this_build_does_not_have_names_it_and_exits_three` | `power.freq` `bpf:tp` pin exits 3 | the same branch |
| `tests/scenario_power.rs:1830` `the_three_refusals_are_told_apart_by_what_they_say` | the build refusal is exit 3 | the build refusal reached through a pin whose requirements hold |
| `tests/scenario_mem.rs:1593` `forcing_a_backend_with_no_collector_exits_three_and_names_it` | `mem.faults` `bpf:tp` pin exits 3 | the same branch |
| `tests/scenario_mem.rs:2942` `the_kernel_sources_tell_their_three_refusals_apart` | `mem.page_alloc` forced unimplemented is the build's | the same |
| `tests/scenario_gpu.rs:759` `forcing_a_backend_this_build_does_not_implement_exits_three_and_names_it` | `gpu.engine_busy` `pmu:i915` pin names the backend's build gap | the machine's refusal where `pmu:i915` is absent |
| `tests/scenario_io.rs:1157` `forcing_a_backend_this_build_has_not_written_names_it_and_exits_three` | `io.bytes` unwritten pin exits 3 | the same branch |
| `tests/scenario_io.rs:1027` `a_request_keyed_on_what_the_counter_files_do_not_name_is_refused_with_the_backend_that_does` | the key hint names the `bpf` carrier as unwritten | the hint names it as missing here (`KeyElsewhere::Missing`) when its probes fail |
| `tests/cli.rs:1665` `a_request_keyed_on_what_its_backend_does_not_attach_is_refused_with_the_backend_that_does` | the same, through the CLI | the same |
| `tests/scenario_net.rs` (`net.hwts`, ~2076): a request, not a failing test | already branches on the machine (exit 1 without a readable PTP clock, 3 with one) | add the pinned twin: `--backend force:so_timestamping` exits the same code as the walk on each branch |

These tests are written against `docs/DECISIONS.md`'s settled line *"Valid, and not
built" is exit 3, wherever it is discovered* (`docs/DECISIONS.md:127`), and
`scenario_proc`'s message says it outright: *"`force:` bypasses the resolver so
no machine can turn it into any other answer"*. The brief's decision narrows
that line: a backend both unbuilt **and** unservable here is the machine's, on
every path. The settled table needs both lines, the old one re-worded.

Merging `1aa114f` alone would have left this branch red, and stream io rebases
on core, so it is reverted (`4c13181`) and ready to cherry-pick beside the test
changes. The patch for `src/collect/mod.rs`, shown passing over `1aa114f`:

```diff
     fn a_filter_the_backend_pushed_down_is_not_run_a_second_time() {
-        let resolved = resolved_with_filter("io.latency", Some("bpf:tp"), "bytes > 4096");
+        // `bpf:tp` has no collector, and a pinned backend that lacks something
+        // here is refused for the machine first (the chain's order), so its
+        // requirements are cleared: the subject is the placement, not the probe.
+        let mut entry = Catalog::embedded()
+            .unwrap()
+            .get(&"io.latency".parse().unwrap())
+            .unwrap()
+            .clone();
+        for backend in &mut entry.backends {
+            backend.requires.clear();
+        }
+        let catalog = Catalog::from_entries(vec![entry]).unwrap();
+        let mut request = resolved_with_filter("mem.rss", None, "comm == 'x'").request;
+        request.source = "io.latency".parse().unwrap();
+        request.mode = crate::request::Mode::Trace;
+        request.filter = Some("bytes > 4096".parse().unwrap());
+        request.backend = Some("force:bpf:tp".parse().unwrap());
+        let capabilities = crate::capabilities::Capabilities::detect();
+        let resolved =
+            crate::resolve::resolve(&catalog, &capabilities, &request).expect("resolves");
```

## 3. The memory crossing at paranoid 2 (`tests/cross_memory.rs`)

The skip was not the machine's. `withheld` looked for `perf_event_paranoid`
anywhere on stderr, and `mem.faults` names the sysctl in its caveat on every
run, so a run that was **served** (in user mode only) was skipped as *"this
machine withholds a source it needs: caveat: ..."* -- the same defect
`scenario_mem` fixed in `843b46f`. Now a run that exited 0 is never a skip, and
a refusal's skip line starts at its `error:` line.

The fault test reads the run's own note (`faults_in_user_mode_only`: *"page
faults in user mode only"* or the counted path's *"would only open
`mem.faults` for user mode"*) and says so on its table line. It keeps both
bounds there, and the reason is in the test: every byte of a large block is
written by `ctypes.memset` in the program's own code, so every fault inside a
range is one its user mode took; a fault the kernel takes on the program's
behalf could only fall outside the ranges, where nothing counts it. The other
three tests of the file read nothing paranoid changes.

**Shown**: `a_served_run_is_not_withheld_by_its_caveat` (a served stderr with
the caveat is not withheld; a refusal is, from its `error:` line); the whole
file passes at paranoid -1 here. **Not shown**: a run at paranoid 2 -- see the
privileged runs below.

## Review

The fix round answered an adversarial review of `418c275`:

- **`withheld` skipped a crash** (defect, fixed in `af14f63`). A failed run
  with no `error:` line fell back to the whole stderr, where `mem.faults`'s
  caveat names `perf_event_paranoid` on every run, so a panic or a signal
  read as the machine withholding a source. Every refusal `omn` prints starts
  with `error:` (`src/main.rs`); without one, `withheld` now returns `None`
  and the caller's assert reports the crash. **Shown**:
  `a_served_run_is_not_withheld_by_its_caveat` gains an exit-101 panic and a
  SIGKILL, both with the caveat, neither withheld.
- **`stack` ignored weight** (gap, fixed in `223542e`). `stack_counting`
  added one value per row, so `sched.futex`, `sched.lock` and `sched.offcpu`
  stacks were short by each row's count. It now goes through `weighed`;
  `StackCount::events` stays the row count, as `topk`'s does. The comment in
  `offcpu.rs` ("adds up, in `rate`, `topk` and `stack`, to exactly
  `total_ns`") is true from this commit on. **Shown**:
  `emit::tests::a_stack_total_weighs_its_rows`.
- **`summary`'s mean above its max** (gap, fixed in `223542e`). With one row
  of 300 reads in the 16384 ns bucket and 6 ms in all, the mean was 20 000
  and the max 16 384. `max` is now raised to a row's exact mean where that is
  higher (`emit::highest`), in `summary` and `hist`. It stays a lower bound on
  the largest measurement, now never below the mean. **Shown**:
  `emit::tests::a_kernel_built_row_s_max_is_never_below_its_mean`; the
  two-bucket test's `hist` max is now 50 000 (the top row's mean), not 32 768.
- **The run notes overstated the loss** (nit, fixed in `223542e`).
  `runqlat`, `qdisc`, `softirq`, `syscall` and `stall` now say the value, the
  buckets and `min` are bucketed, `max` is a lower bound, and the sum, mean,
  rate, topk and stack totals come from `total_ns`. No test quotes the old
  wording; `docs/scenarios/sched.md:635` quotes it as an earlier run's output
  and is left as it was.
- **The handoff's counts** (nit, fixed here): eleven failing tests, not
  twelve (the `net.hwts` row is a request); `sched.offcpu`'s value is the
  row's mean, not a low bound.
- **`user_mode_only` only labels** (nit, left as it is). Both bounds are
  asserted on every path on purpose, and the lower bound at paranoid 2 is
  reasoned, not run (§3). The privileged run below is what turns it into
  something shown. If it fails there, the fix is the one §3 names: keep only
  the upper bound when `user_mode_only` is set.

## 4. `docs/portability.md:184`

`sched.switches` moved out of the "refused below 6.1" row into its own: only
its `bpf:raw_tp` backend says `kernel>=6.1`, and the sixteenth wave's sweep
found the source usable on 5.15 through `pmu:sw`.

## 5. The GPL-2.0 text

`LICENSES/GPL-2.0-only.txt` is the SPDX text (`/usr/share/licenses/spdx/`
here). `make install` installs it at `share/doc/omn/LICENSES/GPL-2.0-only.txt`
and `make uninstall` removes it and the directory; `scripts/release.sh` lists
and packs it. The README's licence section names the file and where it ships.
**Shown**: `make dist-verify` -- *"contents: exactly the 9 paths `release.sh
list` names"*, and every installed-tree check passed; `make -n install` and
`make -n uninstall` print the new lines.

## The suite

The last `make check` (at `4c13181`) failed one test,
`recorder::a_recording_is_held_to_its_budget_over_a_rolling_span_not_the_whole_run`
(*"the workload wrote its report: No such file"*), which also failed alone once,
at a load average of 37 on 24 threads -- its workload must spin 3 s of its own
cpu inside a 6 s session. It is in the flaky tally
(`docs/open-findings.md:7304`). The `cargo test --workspace --no-fail-fast` run
that followed, at a load of 6: **2163 passed, 0 failed, 10 ignored**; clippy
`-D warnings` and `fmt --check` clean.

Over `1aa114f` (before the revert), two more failed and passed alone once each:
`scenario_net::every_datagram_sent_to_a_closed_port_is_a_drop_with_one_reason`
(in the tally) and `unwind::fp_leaf_recovers_the_caller_of_a_frameless_leaf_and_marks_it`
(not in it; 61 of 2064 samples short of their path, once).

**The fix round** (at `223542e`). `make check` stopped at its first failure,
`collect::tests::the_runner_closes_the_window` (*"9883201 ns early"*, in the
tally at `docs/open-findings.md:8046`; `tests/budget.rs::the_extra_cpu_a_faster_poll_costs_...`
failed beside it, in the tally at `docs/scenarios/io.md:706`), at a load of
15. The `cargo test --workspace --no-fail-fast` run that followed, at a load of
21 to 25 with other streams' suites running: **2364 passed, 14 failed, 11
ignored**; clippy `-D warnings` and `fmt --check` clean. Every failure but one
passed alone at the first rerun (`family_sched` x2, the second a poisoned cpu
lock from the first; `scenario_gpu`, `scenario_io` x2, `scenario_net`,
`scenario_proc`, `scenario_runtime`, `triage` x2, `scenario_sched` x2,
`unwind` x2). Before that, `collect::mem::footprint::tests::the_rollup_agrees_with_the_per_mapping_walk`
failed once in `cargo test --lib collect::` and passed alone; it is not in the
tally.

The one that kept failing,
`scenario_sched::sharing_one_file_is_the_only_difference_between_these_two_runs`
(*"the run attributed a wait to pid N, which is not one of the 12 it
started"*), is two worktrees colliding, not this change: the session filters on
`comm == omnlockprobe`, and while it ran, `ps` showed `omnlockprobe` writers
from `omniscient-w17-engine/target/tmp/` and `omniscient-w17-symbols/target/tmp/`
running the same test. It reads raw events and no emit. **Filed for the
coordinator**: the probe's comm is fixed, so two checkouts running
`scenario_sched` at once see each other's writers; a per-run name, or the test
filtering on the pids it started, would make it hold under parallel streams.
Run it alone after the merge.

## Privileged runs wanted

Alone, by the coordinator, after `make` in this worktree:

```console
sudo -n sysctl -w kernel.perf_event_paranoid=2
cargo test --test cross_memory -- --nocapture 2>&1 | grep -E 'SKIPPED|eight large|test result'
cargo test --test scenario_mem a_counted_run_of_page_faults -- --nocapture
sudo -n sysctl -w kernel.perf_event_paranoid=-1
```

Expected: no `SKIPPED` line, and *"the eight large ranges, evidence KEY, faults
sampled in user mode only (perf_event_paranoid 2)"*. If the lower bound fails
there, the memset reasoning in §3 is wrong and the test should keep only the
upper bound at paranoid 2.

## Decision lines for `docs/DECISIONS.md`

- **The GPL-2.0 text ships** (moves from "Open"): `LICENSES/GPL-2.0-only.txt`,
  installed under `share/doc/omn/LICENSES/` and packed by `scripts/release.sh`,
  because `src/bpf` is `MIT OR GPL-2.0` and an option whose text is absent is
  half an offer.
- **A weighted row's exact sum wins over its bucket bounds**: when a row
  declares a weight and carries `total_ns`, every emit that adds a duration up
  (`hist`, `summary`, `rate`, `topk`, `stack`) takes `total_ns`
  (`emit::SUM_NS_ATTR`); the buckets and `min` stay the bucket bounds, and
  `max` is raised to a row's exact mean so it is never below the mean. Because
  `value * weight` is a lower bound nothing marks as one.
- **One order for "unbuilt" and "missing"** stays in "Open" until `1aa114f`
  lands with its tests (§2). When it does: *a backend both unbuilt and missing
  a requirement here is the machine's (exit 1) on every path -- the walk, a
  pin, and the key hint*; and the settled *"Valid, and not built" is exit 3,
  wherever it is discovered* gains *"... on a machine that lacks nothing it
  needs"*.

## Needs from another stream

- **Coordinator** (`src/resolve.rs` re-filed, and the eleven tests in §2,
  in files no stream of this wave owns, except `tests/scenario_io.rs`, which
  is io's): cherry-pick `1aa114f` with the test changes in one merge.
- **io** (`tests/scenario_io.rs`): the two rows of §2's table that are yours,
  when `1aa114f` lands; optionally an assertion that the kernel histogram's
  `sum` now matches the stream's within the edge-crossers' skew.
- **Coordinator** (`docs/install.md:33`): the installed-tree listing gains
  `share/doc/omn/LICENSES/GPL-2.0-only.txt`.
- **Owner of `tests/scenario_net.rs`**: the pinned twin of the `net.hwts`
  refusal (§2, last row), once `1aa114f` lands.
