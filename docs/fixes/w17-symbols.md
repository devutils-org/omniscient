# The seventeenth wave, stream symbols

A perf map is a symbol source. `cpu.profile` -- and every stack a
`Symbolizer` names -- reads `/tmp/perf-<pid>.map` when the target wrote one,
and each frame says where its name came from.

## Landed

1. **Perf-map names** (`src/stack.rs`). An address in an *anonymous*
   executable mapping (an unnamed one, or `[anon:...]`; never `[vdso]`, never
   a deleted file) that no file names and that a map line covers takes that
   line's name, at its offset from the line's start. The map is read as it
   grows (append-only, a half-written last line waits for its newline, a later
   line over an earlier range replaces it, a shrunk file is a replaced map and
   is read from its start, and so is another file at the same path -- a
   recycled pid, a restarted runtime -- told apart by its device and inode
   even when it is as long as the old one):
   - on a miss, at most every 10 ms (`PERF_MAP_REREAD_NS`, the cadence
     `runtime.jit` reads the same file at), and once after each `refresh`;
   - on every `Unwinder::refresh_symbols` (each `cpu.profile` tick) once a
     frame in anonymous code has gone unnamed -- an unwinder that never met
     one opens nothing -- and when a read added lines, the frames interned
     before their line was written are named again
     (`Symbolizer::rename_from_perf_map`);
   - once more at the end by `Unwinder::finish()`, which `cpu.profile`'s and
     `mem.ibs`'s `stop` now call on every unwinder after the last drain (fix
     round: two small hunks in `src/collect/cpu/profile.rs` and
     `src/collect/mem/ibs.rs`, files no stream owns this wave -- the
     coordinator may want to look at them at the merge).
   Read under `--symbols local` (and so `debuginfod`), as decided; the
   `SymbolSource::Local` doc says so.
2. **Every frame says where its name came from**: `Frame::named_by`, one of
   `elf`, `debuginfod`, `kallsyms`, `perf-map`, absent exactly when the frame
   has no name. Per frame rather than per module, because a debuginfod-named
   module also carries its own `.dynsym` names (`Function::from_debuginfo`).
   The per-module record gains `NameOrigin::PerfMap { lines }`, keyed by the
   map's path, and the facts line reads *"names came from -- perf-<pid>.map:
   perf-map, the 172 lines the program wrote for code no file holds; ..."*.
3. **What it could not name is a concession with its reason**: unnamed frames
   in anonymous executable memory are a note, saying either that the map does
   not exist (and which flag writes one: `python3 -X perf`,
   `node --perf-basic-prof`, a JVM agent), that it holds bytes the run never
   read (lines written after its last read, compared by length and identity
   against what was consumed), or -- only when it was read to its end -- that
   it covers none of them;
   undecoded map lines and a replaced map are notes too.
4. **One parser** (`stack::PerfMapLine`): `runtime.jit`'s `Entry::parse`
   (`src/collect/runtime/jit_map.rs`) now reads its address, size and symbol
   through it and adds only V8's code kind. Its ten tests pass unchanged.

## Shown versus reasoned

Shown, by tests on this machine (x86-64, kernel 7.2, Python 3.14.7, paranoid
-1 at the time; the tests use user-only callchains so paranoid 2 serves them):

- `tests/stack_capture.rs::code_a_program_generated_is_named_by_the_perf_map_it_wrote`
  -- a Rust program mmaps a page, writes `mov rcx, rdi; dec rcx; jnz; ret`
  into it, names it in its perf map and burns there. The reference is the map
  read back from disk: every sample in the line's range carries the line's
  name, `named_by: perf-map`, at `addr - start`; more than half the samples
  are there; the facts name the map.
- `a_perf_map_written_at_exit_names_the_frames_when_the_run_finishes` -- the
  same program writing its map only at exit: the frames are unnamed during
  the run and the note says the map does not exist; after the exit,
  `Unwinder::finish()` names them from the map.
- `a_perf_map_written_at_exit_names_the_frames_of_an_omn_profile` (fix
  round) -- the late fixture through `omn cpu.profile`: the stacks carry
  `omn_jit_spin` and no unnamed-anonymous concession remains anywhere in the
  result. Shown to fail with the `finish()` call in `cpu.profile`'s `stop`
  taken out (the stack ends `0x7fa3e2330000+0x3`). The in-process test above
  also checks that, after the exit and before `finish()`, the reason given is
  "holds N bytes this run never read", never "covers none of them".
- `a_profile_through_omn_names_generated_code_from_its_perf_map` -- the same
  through `omn cpu.profile --emit stack`: the stack table carries
  `omn_jit_spin` and the notes name `perf-map`.
- `a_python_function_is_named_in_the_profile_under_x_perf` -- `omn
  cpu.profile -- python3 -X perf hot.py`: the interpreter's own map holds
  `py::omn_hot_loop:<script>`, and more than half of the samples, weighed by
  events, carry that frame. `/usr/bin/python3` here has no frame pointers, so
  `--stack auto` chose `dwarf`; the trampoline is unwound through. Skipped
  loudly where `sys.activate_stack_trampoline` is absent.
- Unit tests in `src/stack.rs`: the line parser, `is_anonymous`, the map read
  as it grows (partial line, undecoded line, overlap, replacement), a frame in
  anonymous code named by the map, a frame named again after its line
  appears, and the `named_by` spelling in JSON.

Reasoned, not shown:

- The off-CPU sources (`sched.offcpu`, `sched.lock`, `sched.futex`) build
  `Symbolizer::for_process` and get the on-miss read (timed, so it works
  without refreshes), but not the retroactive renaming, which lives in
  `Unwinder`. No test runs a JIT under them.
- `node --perf-basic-prof` and a JVM agent: not installed here; the parser is
  the one `runtime.jit` already held to V8's and a JVM's line shapes.
- The cost: a run with no anonymous code opens nothing per tick (the refresh
  reads the map only once an anonymous frame went unnamed), one `open` at
  `stop` per unwinder, and nothing per frame. Not measured with `scripts/bench.sh`
  (not run, as the brief says).

## Not done, and why

- **A JIT that double-maps its code through memfd is not covered.** .NET with
  `DOTNET_PerfMapEnabled` executes from `/memfd:doublemapper (deleted)`, a
  mapping with a path, so `is_anonymous` says no and the map is never
  consulted for it. Not asked by the brief; a later wave can add `/memfd:`
  mappings to what a perf map may name.
- **The off-CPU sources do not call `finish()`**: they hold a `Symbolizer`,
  not an `Unwinder`, and so neither rename retroactively nor at the end.
- The "interpreter frames" wording a CPython run without `-X perf` should
  produce is the triage's and the python stream's: without `-X perf` there is
  no anonymous code at all, so this layer has nothing to say about it.

## Privileged runs wanted

None. Every test here runs unprivileged; at `perf_event_paranoid` 2 the
in-process ones sample user stacks only, which is all a perf map names. A
confirmation at paranoid 2 would be welcome while the coordinator has the
machine at 2 for another reason:

```console
cargo test --test stack_capture -- perf_map python
```

## Decision lines for `DECISIONS.md`

- **A perf map is read under `--symbols local`** (settled, 2026-10-03, stream
  symbols): `/tmp/perf-<pid>.map` is a file the target wrote on this machine
  and asks nothing of anyone, so it needs no rung of its own; it names only
  addresses in anonymous executable mappings, never one a file already names.
- **A frame says where its name came from** (settled): `named_by` per frame
  (`elf`, `debuginfod`, `kallsyms`, `perf-map`), because one module can be
  named by two sources and one stack by four; the per-module `NameOrigin`
  stays for the facts line.
- **A later map line over an earlier range wins** (settled): code freed and
  generated again at the same place is the new code; the overlapped line is
  dropped rather than kept beside it.

## Needs from another stream

- **Coordinator (`src/collect/cpu/profile.rs`, `src/collect/mem/ibs.rs`)**:
  the fix round wired `unwinder.finish()` into both `stop`s itself (after the
  last drain), since the review counted its absence as a gap; please check
  those two hunks at the merge rather than adding them again.
- **python stream**: frames named from the map read `py::<function>:<file>`
  in the stack table, and the facts line carries `perf-<pid>.map: perf-map`.
  A verdict can say how it knows the name from that line, or from
  `Frame::named_by` where it holds `Stack`s rather than labels.
- **Coordinator (`docs/unwind.md`, `CATALOG.md`'s `--symbols` lines)**: add
  the perf map to what `--symbols local` reads.

## The suite

Fix round: one `make check` on the fix commit, red at
`tests/scenario_io.rs::a_histogram_built_in_the_kernel_is_the_one_the_streamed_requests_make`
(*"4 requests of neither size the program read at"*), with the load average
at 15 to 23 from the other streams. The one `cargo test --workspace
--no-fail-fast` then: **2160 passed, 12 failed, 10 ignored**. Each of the
twelve was rerun alone once; eleven passed, among them the one stack-related
(`scenario_sched::a_targets_blocking_stack_is_named_while_another_process_floods_the_stack_map`).
The twelfth, `scenario_sched::sharing_one_file_is_the_only_difference_between_these_two_runs`
(*"the run attributed a wait to pid N, which is not one of the 12 it
started"*), fails alone as well -- and fails the same way with this round's
changes stashed, so it is not this round's; it involves no stack or perf map.
Worth a look by whoever owns `sched.lock`. `stack_capture` (13) and the
`stack` unit tests (62) green; clippy and fmt clean.

The first round's:


One `make check` on `aa9c4ca`: red at `tests/cross_io.rs::the_crossing_sessions_run_and_say_what_they_found`
(*"the one-liner issues 2000 direct reads and the scoped join matched 4000"*),
in the flaky tally (`docs/open-findings.md`, the ninth wave's addition), with
six agents driving the machine. The one `cargo test --workspace --no-fail-fast`
then: **2168 passed, 3 failed, 10 ignored** -- `scenario_proc::defect_1_the_first_reading_of_a_launched_target_is_of_the_program`,
`scenario_sched::an_overflowing_wakeup_ring_says_what_it_could_not_hold`
(in the tally), `triage::a_verdict_read_off_a_lossy_profile_says_its_amount_is_a_lower_bound`;
none touches a stack, and each passed alone at once. `cross_io` passed in the
no-fail-fast run. `cargo clippy --workspace --all-targets -- -D warnings` and
`cargo fmt --check` clean. `stack_capture` (12) and `unwind` green in both runs.

## Review

The fix round answered the adversarial review:

- **Defect, the wrong reason for an unnamed frame** (`src/stack.rs`): fixed.
  The reason now comes from whether the map was read to its end
  (`PerfMap::unread`: the file's length against `consumed`, plus a pending
  half line, or the whole file when its device and inode changed), not from
  whether it exists. A map written after the last read says it "holds N bytes
  this run never read"; "covers none of them" is said only of a map read to
  its end.
- **Gap, no collector called `finish()`**: fixed in `cpu.profile` and
  `mem.ibs` (see above), with an end-to-end test through `omn` that fails
  without it.
- **Gap, a fixture build failure skipped as "not x86-64"**: fixed. A failed
  `rustc` build of the JIT fixture now fails the test with rustc's stderr;
  only a non-x86-64 machine skips.
- **Nit, skip wordings not in `.github/expected-skips*.txt`**: left out on
  purpose. The file holds what the cell lacks, not workloads the workflow
  installs; CI installs Ubuntu 24.04's python3 (3.12), which is built with the
  perf trampoline on x86-64, and the JIT fixture is x86-64 like the runner.
  "this machine withholds cpu.profile" quotes omn's refusal on its following
  lines, which the gate already reads (the `perf_event_paranoid` patterns of
  the stock cell). Not checked on a runner: if the gate trips on one of them,
  the wording it names is the line to add.
- **Nit, a replaced map seen only when it shrinks**: fixed (device and inode,
  unit-tested with a longer file renamed into place). **`perf_map_named`
  counting per symbolization**: the doc now says so; it is read only as
  whether the map named anything.
- **Nit, memfd double-mapping**: a line under "Not done".
- **Nit, an `open` of `/tmp` per tick per unwinder**: fixed; the refresh
  reads the map only once a frame in anonymous code went unnamed.
