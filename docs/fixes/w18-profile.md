# The eighteenth wave — stream profile

Brief: `docs/dev/BRIEF.md`, "profile -- what a profile costs, and the ladder". Files
owned: `src/collect/cpu/profile.rs`, `src/unwind.rs`,
`src/collect/mem/page_faults.rs`, `catalog/unwind.yaml`,
`tests/{unwind,stack_capture}.rs`, `docs/unwind.md`. The long form, with the
tables, is `docs/unwind.md`, "The eighteenth wave: what a profile costs".

## What landed

| commit | what |
|---|---|
| `3975c3c` | `fix(profile)`: a process's mappings are re-read only in a tick whose drain holds samples of it; the user-only degradation says what such a profile costs |
| `dada897` | `fix(profile)`: corrected after the suite -- what is being sampled is re-read before the drain, as before, so an `execve`'s first samples keep their names |
| `c015fc0` | `test(profile)`: `mem.faults`' sampled rung on the paranoid-2 path, weighed; the cost record's whole account held to `wait4` from both sides |
| `e3e53bb` | `docs(unwind)`: the measurements below |

### 1. `cpu.profile` at paranoid 2: 266 %, found

**Which mode.** None is walked: the triage forces `--stack fp` at
`sample@997hz` (`src/triage/latency.rs`), so at paranoid 2 it is `fp`,
user-only.

**Why it costs that.** Two costs, and neither is the mode's:

1. *A fixed cost per tick.* Every tick re-read every followed process's maps
   (twice with CFI) and its executable's link, whatever the drain held: 0.25
   to 0.4 ms a tick of a debug build. A user-only profile of a program that
   lives in system calls gets few samples (21 to 55, against 500 to 640 with
   both halves, for the same 500 commits) and paid for every tick anyway.
   **Fixed** (`3975c3c`, corrected by `dada897`): a process the last drain
   held samples of, or that no drain has held samples of yet, is re-read
   before the drain as before; one idle and sampled again, after the drain
   and before its stacks are interned, and not again before the next drain
   (review round); an idle one, never, once a drain has held samples of it. The first version
   read every sampled process after the drain, which across a launched
   program's `execve` left 82 to 266 frames in no mapping at 100 kHz where
   the old rule left 7 to 54 (the suite found it: `tests/triage.rs`'s lossy
   profile carried 81). The corrected rule is back at 10 to 223 against 10
   to 238, eight runs each.
2. *A one-time cost at the first sample.* The first sample in a module named
   from debuginfod loads that module's symbols (`src/stack.rs`), about 25 ms
   of a debug build for `sqlite3`'s debug file, inside the window. The
   budget's first check, 0.1 s in, weighs it against a target that has burnt
   13 to 21 ms: 116 to 179 %, **with both halves as well as user-only**. That
   is the 266 %. Not fixed: neither file is this stream's (filed below).

**Brought into its band, or the band declared?** Neither, fully. User-only,
fsync-bound commits, seven pairs run alternately at load 22 to 27, no
debuginfod: 13.2 to 58.2 % before, **7.1 to 28.7 % after**, separating in each
pair. Still well above
`low` (under 1 %) for a debug build against a program that mostly waits; the
`cost:` line is the bench's, so the measurements are filed there. The
resolver's choice says it: the user-only degradation now ends *"a program that
spends most of its time there yields few samples, and what the profile costs
it is then mostly the tool's work on each tick, which does not shrink with
them"*.

Base against this stream, measured by checking the base `profile.rs` out and
`make`: `triage::a_verdict_read_off_a_lossy_profile_says_its_amount_is_a_lower_bound`
fails on both (three of three on base, two of two here, at load 22 to 35): the
100 kHz profile is throttled 333 times and the verdict, undecided with 214 to
284 ms of wall clock in no row, names no limit. Not this stream's; filed below.

### 2. `mem.faults`' ladder where it is served

The move `trace` to `sample` on weighed samples was held only by
`tests/budget.rs`, where the kernel's half is allowed (green here: it moved and
held the task's count). The rung on the paranoid-2 path is now held in
`page_faults.rs`:
`sampled_in_user_mode_the_weighed_samples_are_the_faults_the_thread_took`.
256 samples weighing 4096 faults where the thread took 4096, three times out of
three.

### 3. `collector_cpu_ns`

`each_mode_copies_...` now holds `before_window_cpu_ns + collector_cpu_ns +
after_window_cpu_ns` to `wait4`'s count beyond the target, from both sides
(never above, at least half). Measured: 1169.9 ms said against 1178.1 ms
counted (`fp`), 1150.9 against 1159.5 (`dwarf`). The window alone said 55.5
and 72.3 ms. The fourteenth wave's gap (26 ms against 986 ms) is closed:
it was before the window, which the record has carried since the sixteenth
wave.

### 4. `unwind`'s stripped build, the second shape

Not reproduced in 22 runs alone at load 22 to 27, then **seen once** in the
workspace run at load 30 to 48: the unstripped reference's 299 samples held
only the `hash_block` and `parse_record` branches, where the stripped run
beside it held all four. The program's own work, not its names: a whole
branch absent from a profile of an unchanged binary is a branch that did not
run while it was sampled, or ran outside the window. Not this wave's to fix
without a run that shows which: for the flaky tally, with that message.

## Review

The adversarial review's seven points, and what became of each (commit
`fix(profile): the review's round`):

- **Gap: machine-wide (`--cpus`) profiles read more than before. Fixed.** A
  process read after drain N (idle in N-1, sampled in N) was in `recent` and
  read again before drain N+1, even when N+1 held nothing of it: two reads
  where the old machine-wide rule read one. The rule is now a type of its own,
  `MapReads`. A process read after a drain, or whose unwinder was built during
  it (`Unwinder::new` reads the maps), is not read again before the next one.
  A process sampled now and then is read once per drain that holds it, and a
  process held by consecutive drains is read once a drain. Held by
  `over_a_machine_a_process_is_read_once_for_each_drain_that_holds_it`, which
  drives `MapReads` the way `drain_into` does over ten drains of four
  processes, one of them first seen mid-run. With the reviewer's rule
  restored, that test fails (process 10: 6 reads for 4 drains). The launched
  test still reads 25 and 26 times over 81 drains, with 56 and 57 of them
  empty.
- **The struct's doc comments. Fixed.** `refreshes` and `idle_drains` have
  their own docs. The rule's doc sits on `MapReads`. `unwinders` no longer says
  "re-read every tick".
- **82 to 172 against 82 to 266. Fixed.** It now reads 266, as here and in
  `docs/unwind.md`.
- **A target that is never sampled is still read every tick. Kept, and
  said.** That is what the `execve` correction needs (`dada897`). A launched
  program's maps change under its first samples. `MapReads`' doc,
  `docs/unwind.md` and section 1 below now say that "idle" means "once a
  drain has held samples of it". A program that only sleeps, or that lives in
  system calls under a user-only profile until its first user-mode sample,
  costs what it did under the old rule.
- **The records are gathered before they are processed. Kept, and said.**
  Which processes a drain holds decides what is read before any stack is
  interned, so the whole drain has to be seen first. `drain_into`'s doc now
  gives the price: one tick holds the rings' whole contents at once. With
  `dwarf` on per-cpu rings at a high frequency, that is every stack copy the
  rings held. Not measured.
- **The degradation's text untested. Fixed.** The text is now built by
  `unwind::user_only_degradation`, and
  `a_user_only_choice_says_what_it_loses_and_what_it_costs` checks it from a
  paranoid-2 environment on any machine, for a stack mode and for `none`.
- **The new test's skip gate. Fixed for this test.** It now skips only where
  `SampledEvent::callchain_supported` fails, meaning the kernel refuses a
  sampled event with a stack of either shape. A `start` that fails where one
  opens now fails the test. The older test with the same pattern
  (`a_profile_of_the_machine_...`) predates this stream and is left as it
  was.

## Shown versus reasoned

**Shown, on this machine**:

- The 266 % is the first budget check weighing the debuginfod symbol load:
  `cpu.profile --stack fp --symbols debuginfod --budget 100%` over the case's
  500 fsync-bound commits stopped at 0.1 s at 129 to 179 % with both halves
  and 116 to 131 % user-only (one user-only run of three escaped the first
  check and finished at 9 %).
- The per-tick cost and its fix: the separation above, and a temporary
  counter that measured the refresh at 55 to 180 ms over 2.4 s runs. The
  unmapped frames at 100 kHz, base against the corrected rule, eight runs
  each: 238 10 38 138 64 46 49 29 against 40 12 25 223 19 24 41 10.
- With both halves, the fix does not separate (20 to 33 % before, 20 to 30 %
  after): the drain dominates there, about 150 µs a sample of a debug build,
  most of it naming kernel frames.
- The two new tests, green; the whole-account bound, and the weighed rung.

**Reproduced by a patch, not by the machine.** The brief's reproduction (a
capability-less copy of `omn`) does **not** take the user-only path here:
this machine is at paranoid -1, which gives every user the kernel's half. The
user-only runs above used a build whose `kernel_callchain_allowed` answered
no on an environment variable. Not committed:

```diff
-    *ANSWER.get_or_init(|| probe_open(Callchain::BOTH_HALVES).is_ok())
+    *ANSWER.get_or_init(|| {
+        std::env::var_os("OMN_W18_USER_ONLY").is_none()
+            && probe_open(Callchain::BOTH_HALVES).is_ok()
+    })
```

**Reasoned only**:

- What paranoid 2 adds: the kernel refusing the probe rather than the patch
  answering no, and nothing to the denominator -- the budget reads the
  scheduler's nanoseconds, which count the target's kernel time whether the
  profile may sample it or not.
- Why the agent's way was green at paranoid -1 in the coordinator's runs when
  the same check stops a run with both halves here: not found. Here both
  halves are stopped too, when `--symbols debuginfod` is asked and the first
  check falls before the target has burnt more than the symbol load.
- The new profile test at paranoid 2: it builds its collectors user-only
  exactly where `kernel_callchain_allowed` says no, so it should run on the
  `defaults` cell; at paranoid 4 it skips with *"refuses unprivileged perf
  sampling"*, a line `.github/expected-skips.txt` already allows.

## What could not be done, and why

- **The one-time symbol load**: in `src/stack.rs` (`Symbolizer`), stream
  sched's file. Filed.
- **A first budget check that stops a run on a one-time cost** against a few
  milliseconds of target: `src/collect/mod.rs`, stream launcher's. Filed.
- **The `cost:` band of `cpu.profile`'s `pmu:sw`**: stream bench's line.
  Filed with the numbers.
- **Both halves' per-sample cost** (kernel frames named per sample): in
  `src/stack.rs`. Filed.

## The suite

While iterating: `cargo test --lib collect::cpu::profile` (9 passed, three
runs, and once with the user-only patch: 27 reads over 81 drains), `collect::mem::page_faults` (4
passed, three runs), `stack::unwind` (5), `--test unwind` (17),
`--test stack_capture` (13), `--test budget a_traced_series_...` (green, moved
and held). Clippy `--all-targets -D warnings` clean.

`tests/unwind.rs::dwarf_recovers_the_whole_tree_of_a_build_without_frame_pointers`
failed once in the full file run at load 48 (line 402: a branch's share of the
samples outside its slack), then passed six times alone. A share under load,
not an unwinding defect: for the flaky tally.

The first `make check` (at `e3e53bb`) went red, and one `cargo test
--workspace --no-fail-fast` at load 30 to 48 counted 2448 passed, 31 failed,
9 ignored. In this stream's files: `unwind` (3, all green alone twice, 17 of
17) -- among them the stripped build's second shape, seen at last: the
unstripped reference holding only `hash_block` and `parse_record` (146 and
153 samples, no `compare`, no `checksum`), the shape the sixteenth wave saw
once; and `dwarf_counts_and_says_the_stacks_deeper_than_its_copy`,
`fp_leaf_recovers_...` (63 of 2027 short, already in the tally). Outside them,
under the same load: `cases` (green alone, 227 s, both ways of
`sqlite3-durability` included), `triage` (the lossy profile, red on base
too), `budget` #14, and `scenario_{gpu,mem,net,sched}`, `walkthrough`, not
rerun.

After `dada897`: one `make check`, red at `budget` #14 only (1493 passed, 1
failed; it stops at the first red binary), and #14 passed alone at once. Then
one `cargo test --workspace --no-fail-fast` at load 58 to 107 (another stream
was running its suite): **2189 passed, 4 failed, 10 ignored**, every binary in
this stream's files green -- `unwind` 17 of 17 included. The four:

- `cases::every_case_names_its_known_cause_both_ways`: four checks, among them
  `sqlite3-durability/every-commit-waits-for-the-disk/mcp` stopped by the
  budget at 117.89 % (`cpu.profile` 149.46 %) -- the one-time symbol load
  above, at paranoid -1 and with both halves -- and the flat-profile suspect
  comparisons already in the tally. Green alone at load 25 before the
  correction (227 s).
- `runner::a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed`:
  a series short by 75 884 where its ring says 75 016. **Red on base too**
  (`9cd5409`'s `profile.rs`, `make`, two of two: 72 565 against 71 736),
  and with `3975c3c`'s (two of two), at load 10 to 70: not this change.
  Filed for the runner's owner.
- `scenario_net::every_datagram_sent_to_a_closed_port_is_a_drop_with_one_reason`
  and `triage::a_verdict_read_off_a_lossy_profile_...` (red on base too,
  above): under that load, not rerun.

After the review's round (`65b7b8e`): `cargo test --lib collect::cpu::profile`
(10 passed), `stack::unwind` (6), clippy and fmt clean. One `make check` at
load 2 to 15 went red at `runner`'s ring-loss test only. That test is red on
base, above (a series short by 82 482 against a ring loss counted lower). One
`cargo test --workspace --no-fail-fast` followed: **2192 passed, 3 failed, 10
ignored**. Every binary in this stream's files was green. The three failures
were all known before this round:

- `runner::a_profile_s_losses_land_...`: red on base.
- `triage::a_verdict_read_off_a_lossy_profile_...`: red on base.
- `cases::every_case_names_its_known_cause_both_ways`: only the flat-profile
  suspect comparison already in the tally
  (`sqlite3-durability/commits-that-wait-for-nothing`, `_raw_spin_unlock_irqrestore`
  against `bpf_trace_run2`). There was no budget stop this time.

## Privileged runs wanted

Run alone, by the coordinator, on this machine at paranoid 2, then set back:

```console
sudo sysctl kernel.perf_event_paranoid=2
cd /home/moha/repos/omniscient && make
cp target/debug/omn /tmp/omn-nocap            # no setcap: the CI's defaults cell
cd "$(mktemp -d -p ~/.cache)"                 # on a disk, not tmpfs
python3 /home/moha/repos/omniscient/cases/sqlite3-durability/commits.py c500.sql 500
sqlite3 delete.empty.db 'PRAGMA journal_mode=DELETE; CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT);'
for symbols in "" "--symbols debuginfod"; do
  cp delete.empty.db delete.db
  /tmp/omn-nocap cpu.profile -m sample@997hz --stack fp $symbols -e topk --budget 100% \
    -- sqlite3 "$PWD/delete.db" 'PRAGMA synchronous=FULL' ".read $PWD/c500.sql" 2>&1 \
    | grep -E 'degraded: stacks|cost:|budget:'
done
cargo test --lib collect::cpu::profile::tests::a_tick_with_no_sample_of_a_process_does_not_reread_its_mappings
cargo test --lib collect::mem::page_faults::tests::sampled_in_user_mode_the_weighed_samples_are_the_faults_the_thread_took
sudo sysctl kernel.perf_event_paranoid=-1
```

Expected: the degradation names paranoid 2 and ends with the cost sentence;
without debuginfod the run finishes under 15 %; with it, the first check
stops it above 100 % until the two filings below land. Then the case itself,
the agent's way, once the mcp stream's single privilege model is merged:
`cargo test --test cases every_case_names_its_known_cause_both_ways -- --nocapture` at paranoid 2, read for `sqlite3-durability`.

## Decision lines for `docs/DECISIONS.md`

- **A collector's per-tick work follows what the tick drained.** Work a tick
  does whatever its samples -- re-reading a process's maps, its executable,
  its CFI -- is paid by a program that waits as much as by one that burns, and
  is most of what a profile costs a program that waits. It is done for the
  processes the drain holds samples of, in the same tick, before their stacks
  are interned.
- **What paranoid 2 costs is said where the choice is made.** The user-only
  degradation says that the samples shrink with the program's user time and
  the tool's per-tick work does not, rather than leaving the reader to infer
  from a percentage why a profile cost more than its band.

## Needs from another stream

- **launcher** (`src/collect/mod.rs`, the budget's check): the first check,
  0.1 s into a launched run, stops `cpu.profile` at 116 to 179 % on a one-time
  cost -- the debuginfod symbol load of the first sample, about 25 ms of a
  debug build -- against a target that has burnt 13 to 21 ms. This, not
  paranoid 2, is `sqlite3-durability`'s 266 % on the agent's way. Wanted: a
  rule for when a check may stop a run on a figure whose denominator is a
  few milliseconds of target -- for example, not before the target has burnt
  some multiple of the check interval, or weighing what a collector reports
  as a one-time load apart -- and the stop's message saying which.
- **sched** (`src/stack.rs`, `Symbolizer`): (1) the first sample in a module
  named from debuginfod loads its whole symbol table inside the window,
  about 25 ms of a debug build for `sqlite3`'s debug file; loading it lazily
  or indexed would take it off the budget's first check. (2) With both
  halves, a profile's drain costs about 150 µs a sample of a debug build, most
  of it naming kernel frames: at 997 Hz that is 15 % of the target's cpu by
  itself.
- **bench** (the `cost:` line of `cpu.profile`'s `pmu:sw`, declared `low`):
  measured here, debug build, `sample@997hz --stack fp`, against 500
  fsync-bound `sqlite3` commits (the target burns about 0.2 s a second): 6 to
  10 % user-only, 20 to 30 % with both halves, after this stream's fix; 4 to
  8 % against 50 000 commits on a WAL (compute-bound), before it. The bench's
  0.61 % is a compute-bound launch. The band holds for a program that burns;
  for one that waits it does not, and the line or its caveat should say
  which.
- **triage** (`tests/triage.rs`):
  `a_verdict_read_off_a_lossy_profile_says_its_amount_is_a_lower_bound` is
  red on this machine with and without this stream's change (five of five, at
  load 22 to 35): at 100 kHz the profile is throttled 333 times and the
  verdict is undecided (*"the run names no single state: `on-cpu` is
  134.41ms, and 214.10ms of the wall clock is in no row"*), so `limit` is
  null where the test expects `cpu`.
- **launcher** (`tests/runner.rs`):
  `a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed`
  is red here at `9cd5409` itself, four of four runs: the series is short by
  about 800 more than its ring reports (72 565 against 71 736). The per-ring
  figure is read at each drain (`ProfileCollector::ring_losses`); the series'
  shortfall comes from somewhere that counts more. Not looked into further:
  it fails identically without this stream's change.
- **green** (`docs/flaky.md`): `unwind::a_stripped_build_...`'s second shape,
  seen once in a workspace run at load 30 to 48 after 22 green alone; and
  `unwind::dwarf_recovers_the_whole_tree_...`, once at load 48 (a branch's
  share outside its slack), six of six alone.
- **coordinator** (`docs/dev/STATE.md` item 11): "the mode ladder where `mem.faults` is
  served" and "`collector_cpu_ns`" can leave the list (sections 2 and 3
  above); "`unwind`'s stripped-build second shape" stays, seen once more
  under load (section 4); the per-tick work and the one-time load are filed
  above.
