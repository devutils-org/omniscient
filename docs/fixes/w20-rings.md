# The twentieth wave, stream rings

Item 4 (`make check` green: `cpu.profile`'s `losses_by_ring` carrying each
ring's throttles) and, from item 11, `unwind`'s stripped-build second shape.

## What landed

- **`bdef096` fix(profile): each ring carries its throttles beside its drops.**
  `src/perf.rs` splits the throttled periods per ring the way it already
  split the drops: `TaskSampler::throttled_by_task` (rings of exited tasks
  kept in `retired_throttled`, now a map by task rather than a sum) and
  `ThreadSampler::throttled_by_cpu`. `src/collect/cpu/profile.rs` merges the
  two accounts per key (`RingLost::merge`) and `losses_by_ring` reports a
  ring that dropped, was throttled, or both, each with its own note. Before
  this, `emit.rs`'s `placed` saw the rings' throttles sum to 0 against the
  run's thousands, refused to place, and marked every series with the whole
  run's loss -- the reason `runner::a_profile_s_losses_land_...` was red in
  every run since the eighteenth wave.
  `tests/runner.rs`: the test now holds each series to its own ring's
  `(dropped, throttled)`, a series whose ring lost nothing to no mark, and
  the rings' sums to the run's (checked first, so a mismatch names the short
  account). Its old last assertion, "marked on fewer than every series", is
  gone: under load the kernel throttles the light task's ring too (measured
  below), so every series can be rightly marked; what is held is that each
  carries its own account and no other's.
  Unit test: `collect::cpu::profile::tests::each_ring_carries_its_throttles_beside_its_drops`.
- **`c7c90c1` test(unwind): sample the stripped build below its shortest
  branch.** `named_stacks` sampled every 1 ms; a batch of the call tree is
  973 us of cpu here (`calltree-nofp 300`: 292.9 ms, 291.1 ms; 1000: 970.9
  ms). One sample a batch at a phase drifting by the difference lets a run's
  samples sit in one or two neighbouring branches -- exactly the recorded
  second shape (reference holding only `hash_block` 146 and `parse_record`
  153). The period is now `NAMED_PERIOD_NS = 61_001` (under `checksum`, the
  shortest branch, about 97 us), so every branch holds a sample point in
  every batch whatever the phase. About 4800 samples a run; the test takes
  1.4 s.

## Shown versus reasoned

Shown:

- `runner::a_profile_s_losses_land_...` passed 6 of 6 at load 10 to 11, none
  of them a skip. One run's numbers, by `omn trace cpu.profile -m
  sample@50000hz -k task`: run `(dropped 71309, throttled 3739)`; rings
  `task 1077530 (70974, 3698)`, `task 1077531 (335, 39)`, `task 1077529 (0,
  2)`; each series' `short` equal to its ring's, so the sums match and
  `emit.rs` placed them. The light task and the waiting shell were throttled
  too -- which is why the old "not on every series" assertion could not hold
  under load.
- Aliasing is real for this workload: `named_stacks` with the period matched
  to the batch (972 us) put 229 of 299 samples in `hash_block` (built 40 %)
  in one run of seven periods tried between 970 and 1000 us; the other six
  were near construction. At 61 us: 4799 samples, `checksum` 481, `compare`
  1430, `hash_block` 1923, `parse_record` 964 (10/30/40/20 % built; 10/30/40/
  20 % measured to a point). `a_stripped_build_...` 3 of 3 green, and in the
  file's whole run twice.

Reasoned:

- That the 1 ms aliasing is *the* cause of the two field sightings of the
  second shape. They were not reproduced on demand (40 runs in earlier
  waves); what is shown is that the mechanism exists and yields the recorded
  shape (two adjacent branches, roughly even), and that a period below the
  shortest branch removes it by construction.
- The share tests (`profile(..., 1999)`: a 500 us period, two samples a
  ~1 ms batch) are exposed to the same aliasing, more weakly. In the first
  concurrent run of `tests/unwind.rs` here, `fp_leaf_recovers_...` (line 389,
  the broken-path share) and `dwarf_counts_and_says_...` (line 626) failed
  once at load 11; both passed alone, and the whole file passed on its
  rerun. Not changed: the fix there is in the workload (a pseudo-random pad
  between batches, outside `spin`, so the phase decorrelates at any rate),
  which moves every share test's runtime and is worth its own measured
  change. Filed under "not done".

## What I could not do, and why

- **The share tests' aliasing** (above): left, with the remedy named. It
  would touch `CALLTREE`, which every test of `tests/unwind.rs` reads.
- `sample_launched` (the cost test) still samples at 1 ms: it counts bytes
  copied and cpu per sample, not shares, so aliasing does not bias it.

## Privileged runs wanted

None beyond what `make` grants (`cap_bpf,cap_perfmon` were on
`target/debug/omn` for every run above).

## Decision lines for DECISIONS.md

- A sampling collector that reads one ring per key reports, per ring, every
  loss the run counts for it -- drops and throttles -- since a placement that
  needs all of them is refused by the one that is missing. (Proposed; costs
  nothing to settle, `perf.rs` now has both splits.)
- A test that holds a profile's shares or paths samples at a period under its
  workload's shortest phase, or with a workload whose phase is decorrelated
  from the period; one sample per loop iteration aliases.

## Needs from another stream

- **shared, or whoever owns `tests/cross_compute.rs`** (in no row; filed, not
  made). **`bdef096` turns one test red, and this is the fix to merge with
  it.** `cross_compute::a_lossy_profile_marks_its_row_and_leaves_the_traced_rows_alone`
  asserted that the decomposed task's histogram carries the *run's* drops
  (18174 in the failing run) -- true only because placement used to be
  refused whenever the kernel throttled. With the throttles on the rings the
  run is placed, and the series carries its own ring's (12054). The test
  should hold the series to its own ring's account, and to the run's only
  when there is no per-ring account. Tried here and reverted: 3 of 3 green,
  none a skip. The patch:

```diff
diff --git a/tests/cross_compute.rs b/tests/cross_compute.rs
index a9912f6..05069dc 100644
--- a/tests/cross_compute.rs
+++ b/tests/cross_compute.rs
@@ -1025,14 +1025,39 @@ fn a_lossy_profile_marks_its_row_and_leaves_the_traced_rows_alone() {
     let short = series_of(profile, "hist", lossy.task())
         .map(|series| series["short"].clone())
         .unwrap_or(Value::Null);
-    assert!(
-        !short.is_null(),
-        "the run dropped {dropped} samples and was throttled {throttled} times, and its \
-         histogram does not say so:\n{}",
-        lossy.text
-    );
-    assert_eq!(short["dropped"].as_u64(), Some(dropped), "{short}");
-    assert_eq!(short["throttled"].as_u64(), Some(throttled), "{short}");
+    // The profile keeps one ring per task, and a series is short by its own
+    // ring's losses when the rings account for the run's (`emit.rs`,
+    // `Shortfalls`); the run's whole loss is on it only when they do not.
+    let rings = profile["by_ring"].as_array().cloned().unwrap_or_default();
+    let (own_dropped, own_throttled) = if rings.is_empty() {
+        (dropped, throttled)
+    } else {
+        rings
+            .iter()
+            .find(|ring| ring["key"]["task"]["tid"].as_u64() == Some(lossy.task()))
+            .map(|ring| {
+                (
+                    ring["losses"]["dropped"].as_u64().unwrap_or(0),
+                    ring["losses"]["throttled"].as_u64().unwrap_or(0),
+                )
+            })
+            .unwrap_or((0, 0))
+    };
+    if own_dropped == 0 && own_throttled == 0 {
+        assert!(
+            short.is_null(),
+            "the decomposed task's ring lost nothing and its histogram is marked: {short}"
+        );
+    } else {
+        assert!(
+            !short.is_null(),
+            "the decomposed task's ring dropped {own_dropped} samples and was throttled \
+             {own_throttled} times, and its histogram does not say so:\n{}",
+            lossy.text
+        );
+        assert_eq!(short["dropped"].as_u64(), Some(own_dropped), "{short}");
+        assert_eq!(short["throttled"].as_u64(), Some(own_throttled), "{short}");
+    }
 
     // The samples the mark says were dropped are time some sampled task spent
     // holding its cpu. Counted back in, they must still fit in the room the
```

- **docs** (`docs/unwind.md`, "The stripped build's second shape"): replace
  "reads as the workload or the window" with the aliasing explanation and
  the change (`c7c90c1`, `NAMED_PERIOD_NS`), and take it off the flaky tally.
- **coordinator** (`docs/open-findings.md`, the nineteenth wave's tally and
  "Not flaky"): `runner::a_profile_s_losses_land_...` is fixed by `bdef096`;
  `unwind::a_stripped_build_...`'s second shape addressed by `c7c90c1`.
  `STATE.md` item 4 and item 11's "`unwind`'s stripped-build second shape".
- **triage**: `triage::a_verdict_read_off_a_lossy_profile_...` was filed as
  red under load wherever it ran; if its verdict reads per-series shortfalls
  of a throttled profile, it may move with `bdef096`. Worth one rerun on the
  merged tree.

## The suite

Clippy (`--workspace --all-targets -D warnings`) and `cargo fmt --check`
clean. Owned files: `tests/runner.rs` 28/28, `tests/unwind.rs` 17/17 (one
earlier concurrent run 15/17, above), `tests/stack_capture.rs` 13/13,
`cargo test --lib perf` 42/42, `--lib collect::cpu::profile` 12/12.

`make check`, once, before the last commit (load 11 to 19): stopped at
`cases` -- `gcc-shell/the-passes-o2-adds/mcp`, `on-cpu` 6434 ms against
`ftime.o2_total_ns` 7410 ms -- after the lib and every test file before it
passed (1621 passed). Then `cargo test --workspace --no-fail-fast` (load 10
to 14): **2547 passed, 2 failed, 12 ignored**; `cases` passed there.

- `cross_compute::a_lossy_profile_marks_...`: **caused by `bdef096`**, the
  fix filed above with its patch.
- `recorder::two_bursts_write_exactly_two_documents_each_holding_its_burst`
  (the value that fired 256 against 303): not mine; passed alone on its
  rerun.
- `cases`' `gcc-shell` mcp figure: not mine (the on-cpu sum against the
  compiler's own clock, at load); passed in the no-fail-fast run.

Note for the coordinator: the session's scratchpad directory is shared by
every stream of the wave, and another stream (shared) wrote `check.log` and
`ws.log` there while this one did; a first suite run here read its numbers
by mistake and was discarded. Logs with a stream prefix, or a directory per
stream, avoid it.
