# The twenty-fourth wave, stream unwind

Branch `feat/w24-unwind`. The brief's three items: `Unwinder::symbolizer`
and `perf_maps`, the unwind share tests' aliasing, `DEVICE_STACKS` as data.
Finding the aliasing turned up a defect, which is fixed first.

## What landed

| Commit | What |
|---|---|
| `3beef38` | `fix(unwind)`: **a copied stack sampled after the target's `execve` is walked by the image it runs now.** `Cfi` read the maps on its first walk and at each tick's `refresh_symbols`. A launched program's unwinder is built at the gate, while the target is still a fork of `omn`, so a walk or a tick that read the maps then registered `omn`'s own modules, and every sample from the `execve` to the next tick ended at its leaf ("stopped early"). A walk whose instruction pointer is in no module added now re-reads the maps first (bounded: eight re-reads that bring nothing, so a JIT's anonymous code does not read `/proc/<pid>/maps` per sample), and a module is kept by start, end and file, so a mapping replaced at the same address (a non-PIE `execve`) is dropped. `--stack dwarf` and `fp+leaf` both walk through it. |
| `b073113` | `feat(stack)`: **`Unwinder::symbolizer()`, `Unwinder::perf_map_file()`, `Unwinder::executable()`**, the accessor the twenty-first wave asked for. The unwinder's own copy of the executable and its private `executable(pid)` reader are gone: the symbolizer holds the same value, refreshed at the same moments. |
| `c1a8847` | `test(unwind)`: **a pseudo-random pad between batches** in `CALLTREE` (`pad`, outside `spin`, a xorshift number of iterations below 2^19, about one 500 us period; `-DPAD_MASK=0` builds without it), batch counts down a fifth (fp 320, the others 800) to keep the runtime; **the exec test** `a_copied_stack_sampled_after_an_exec_is_unwound_by_the_image_the_target_runs_now`. The JIT test in `tests/stack_capture.rs` now also holds `perf_map_file()` and `executable()`; `an_unwinder_no_perf_map_named_reports_none` is new. |

## Shown vs reasoned

Shown, on this machine, under the wave's load (load average 10 to 36):

- **The aliasing, and the pad removing it.** `omn cpu.profile --stack dwarf
  --emit stack` over the `nofp` build of `CALLTREE` at every rate from 1000 to
  1040 Hz (41 runs each). A batch took 982 us of cpu here. Without the pad
  (`-DPAD_MASK=0`, 1000 batches): the worst branch was **22.9 points** off its
  built share (`hash_block` 63 % at 1021 Hz, built 40 %; `parse_record` 4 % at
  1002 Hz, built 20 %), and **8 of 41** runs fell outside the share tests'
  slack (four binomial deviations and two points). With the pad (800
  batches): worst **3.9 points**, **0 of 41** outside. The pad adds 25 % of cpu
  per batch (982 ms against 1232 ms for 1000 batches), which the lower batch
  counts take back.
- **The exec defect.** In the same 41 padded runs before the fix, **110 of
  41246** stacks "stopped early", as single `spin` frames in a few runs (up
  to 5 % of one, which is what turned `dwarf_recovers_...` red once in this
  stream's first run of the file: its 1 % broken-path bound). After the fix,
  the same sweep: **13 of 41832**, and no run with a broken path. The new test
  reads **0 of 1644** samples in `spin` reaching `main` with the re-read
  commented out, and all of them with it.
- **The accessor.** The JIT fixture's perf map is reported with the path,
  device and inode `stat` gives the file the program wrote, the same through
  `symbolizer().perf_map_file()`, and the fixture's binary (canonical path) as
  `executable()`.
- `tests/unwind.rs` 18 of 18, three runs in a row under load; the share
  tests' numbers in one of them: fp over fp 90/258/370/167 of 885 in `spin`,
  fp+leaf 162/485/648/322 of 1617, dwarf 173/469/661/303 of 1606, 0 broken
  (10/30/40/20 % built).

Reasoned:

- That the two fails of the twentieth wave's concurrent run
  (`fp_leaf_recovers_...`'s broken-path share, `dwarf_counts_...`) were the
  exec defect rather than aliasing: a broken path is what the defect yields
  and what aliasing does not (aliasing moves shares, it does not cut walks).
- That the fp build's 885 samples (it is `-O0`, 320 batches) keep enough
  margin over the 500 floor: cpu-clock samples do not depend on load.

## Not done, and why

1. **`perf_maps` in `cpu.profile`'s provenance** (and `executables` from the
   symbolizer): `src/collect/cpu/profile.rs` is stream cpu's. The accessor is
   here; the change is filed below. Not done for that reason.
2. **`DEVICE_STACKS` as catalog data**: the data belongs on the entry, in the
   family's own file (`catalog/gpu.yaml`, stream gpu), read by the loader
   (`src/catalog.rs`, a shared layer) and by `check_stack` (`src/validate.rs`,
   stream emit). None of those is this row's; the unwind table is a list of
   modes, and a list of sources there would put a family's property in
   another file. Filed below with the lines. Not done for that reason.
3. `docs/unwind.md` should say the exec re-read and the pad (it is in no
   row): filed for the coordinator.

## Privileged runs wanted

None. `make` granted `cap_bpf,cap_perfmon` to `target/debug/omn` for every
run above.

## Decision lines

- A walk of a copied stack whose instruction pointer is in no module it
  knows re-reads the target's maps before it gives up, a bounded number of
  times; the tick's refresh is not the only moment the modules are current.
- A workload that holds a profile's shares decorrelates its phase from the
  sampling period by construction (a pseudo-random pad outside the measured
  branches, at least one period wide), rather than relying on a period no
  divisor of the workload.

## Needs from another stream

- **cpu** (`src/collect/cpu/profile.rs`, `fn provenance`): fill `perf_maps`
  and read `executables` from the unwinders, which follows an `execve`:

  ```rust
  let mut perf_maps: Vec<_> = self
      .unwinders
      .values()
      .filter_map(crate::stack::Unwinder::perf_map_file)
      .collect();
  perf_maps.sort();
  perf_maps.dedup();
  crate::stack::Provenance { named_by, perf_maps, executables: self.executables.clone() }
  ```

  and, where `executables` is filled at a task's first sample, prefer
  `unwinder.executable()` (refreshed at each `refresh_symbols`) over
  `stack::executable_of(pid)`, updating the entry at `stop` from the
  unwinder. Shown by: `tests/stack_capture.rs`
  `a_profile_through_omn_names_generated_code_from_its_perf_map` extended to
  read `results[0].perf_maps`.
- **gpu** (`catalog/gpu.yaml`) **and the coordinator** (`src/catalog.rs`):
  an entry-level `stacks: device` on `gpu.profile` (the loader refusing it on
  an entry that does not emit `stack`), exposed as `Entry::device_stacks()`;
  then `src/collect/gpu/mod.rs`'s `DEVICE_STACKS` goes.
- **emit** (`src/validate.rs:782`, `check_stack`): once the field exists,
  `!entry.device_stacks()` in place of
  `!crate::collect::gpu::DEVICE_STACKS.contains(&request.source.as_str())`.
- **coordinator** (`docs/unwind.md`): the exec re-read (`Cfi::reread_for`)
  and `CALLTREE`'s pad; `docs/dev/STATE.md`'s "unwind share tests' aliasing" and
  `Unwinder::symbolizer` lines are answered by this branch.

## Tests run

- `cargo test --test unwind`: 18 passed, four times (one with
  `dwarf_recovers_...` red before the fix, passing alone; three green after).
- `cargo test --test stack_capture`: 14 passed (the JIT tests ran, not
  skipped).
- `cargo test --lib unwind`: 8 passed; `cargo test --lib stack`: 75 passed,
  1 ignored.
- `cargo test --test scenario_cpu` (outside the row, because `cpu.profile`
  walks through `Cfi`): 42 passed.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
  `cargo fmt --check`: clean.

## Review

The reviewer filed one gap: `perf_maps` and `executables` are not wired into
`cpu.profile`'s provenance, and `DEVICE_STACKS` is not catalog data. The
reviewer is right that both are undone, and also agrees that filing them is
justified. The fix round leaves them as they are, for these reasons:

- **Nothing in this row can close them.** After `git merge main` (at
  `63ebae6`), `src/collect/cpu/profile.rs:773` still reads
  `perf_maps: Vec::new()` and `:473` still reads `executable_of(pid)`. The
  change is the one under "Needs from another stream", and it goes in stream
  cpu's file. `DEVICE_STACKS` (`src/collect/gpu/mod.rs:30`, read at
  `src/validate.rs:782`) needs `catalog/gpu.yaml` (gpu), the loader in
  `src/catalog.rs` (a shared layer, which the coordinator lands) and
  `check_stack` (emit). None of those is in the unwind row, and this fix round
  may change only that row and the seams.
- **The open-findings item is half answered, not closed.** "To
  `src/stack.rs`: `Unwinder::symbolizer`" (`docs/open-findings.md`) asked for
  one accessor. The accessor exists (`b073113`) and is tested
  (`tests/stack_capture.rs`). The consumer half stays open until cpu lands
  the `provenance()` change above. The coordinator should note it that way
  when it updates `docs/open-findings.md` and `docs/dev/STATE.md` (neither file is in
  this row): "accessor landed in the twenty-fourth wave (unwind); `perf_maps`
  in `cpu.profile`'s provenance still wanted from cpu". `DEVICE_STACKS` as
  data stays on the list, with the owners named above.

No code changed in this round. Because the commit touches only documents,
the documents-only check ran: `cargo test --test cli`, 28 passed.
