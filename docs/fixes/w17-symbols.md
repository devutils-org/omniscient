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
   is read from its start):
   - on a miss, at most every 10 ms (`PERF_MAP_REREAD_NS`, the cadence
     `runtime.jit` reads the same file at), and once after each `refresh`;
   - on every `Unwinder::refresh_symbols` (each `cpu.profile` tick), and when
     a read added lines, the frames interned before their line was written
     are named again (`Symbolizer::rename_from_perf_map`);
   - once more at the end by `Unwinder::finish()` -- which `cpu.profile` does
     not call yet: see "Needs from another stream".
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
   `node --perf-basic-prof`, a JVM agent) or that it covers none of them;
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
- The cost: a run with no anonymous code pays one failed `open` per tick per
  unwinder (refresh) and nothing per frame. Not measured with `scripts/bench.sh`
  (not run, as the brief says).

## Not done, and why

- **`cpu.profile` does not call `Unwinder::finish()` at its `stop`.** The
  call belongs in `src/collect/cpu/profile.rs` (and `src/collect/mem/ibs.rs`),
  which no stream owns this wave. Until then a map whose last lines are
  flushed after the last tick (a runtime that buffers and flushes at exit)
  leaves those frames unnamed in an `omn` run, with the note saying the map
  covers none of them. CPython flushes each line as it writes it, so the
  Python case is not affected; the in-process test shows `finish()` does the
  work.
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
  after the last drain in `stop`, call `unwinder.finish()` on every unwinder
  (`for unwinder in self.unwinders.values_mut() { unwinder.finish(); }`), so
  the map is read at the end of the run as well as during it.
- **python stream**: frames named from the map read `py::<function>:<file>`
  in the stack table, and the facts line carries `perf-<pid>.map: perf-map`.
  A verdict can say how it knows the name from that line, or from
  `Frame::named_by` where it holds `Stack`s rather than labels.
- **Coordinator (`docs/unwind.md`, `CATALOG.md`'s `--symbols` lines)**: add
  the perf map to what `--symbols local` reads.

## The suite

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
