# ZML profile feedback, 26 October 2026

Scope: the eight observations in `~/dev/NOTE-OMN-DIFF-RETOUR.md`, referenced
by the implementation request, and the existing `runtime.func` implementation
from commit `5da5ddb`. This changes Omn; the ZML benchmark campaign in the
separate research checkout is not rerun here.

| Observation | Result and evidence |
|---|---|
| 1. Repeated trace truncates program logs | `trace -r N` keeps each redirected regular file as `FILE.1` through `FILE.N`, including both streams in one file. The base file holds the last run. Environment variables follow the same rule. Devices such as `/dev/null` remain sinks. `tests/runner.rs`: `repeated_trace_keeps_each_program_output`, `repeated_trace_does_not_copy_a_discard_device`. |
| 2. Local debug information ignored | Stripped mapped ELFs look for matching build IDs in `/usr/lib/debug/.build-id/xx/rest.debug`, then elfutils' existing cache (including `DEBUGINFOD_CACHE_PATH` and `XDG_CACHE_HOME`). No network or cache writes in local mode. `tests/local_symbols.rs` loads a stripped DSO's private function from a cached debug ELF and rejects a different build's ELF. |
| 3. Verdict calls offsets functions and omits named callers | Offsets are instruction rows, not claimed function boundaries; the sentence calls additional changes comparable rows and cites the strongest significant, comparable named inclusive change. `triage::diff::tests::the_verdict_calls_offsets_instructions_and_names_the_inclusive_caller` uses two libc offsets under `CopyThunk`. |
| 4. LLVM/XLA uses jitdump | Binary jitdump names anonymous code from complete load/move records in either byte order. Discovery uses the executable mmap marker's actual path, including custom `JITDUMPDIR`. Parser tests cover partial appends, corrupt records and moves; the symbolizer test checks names and `jitdump` provenance. See `docs/unwind.md` for temporal and unwinding limits. XLA must enable its LLVM PerfJITEventListener. |
| 5. ZML binaries lack build IDs | Offsets without matching recorded build IDs remain non-comparable. This is a producer build setting: ZML needs a linker build ID (for GNU-compatible linkers, `-Wl,--build-id=sha1`, including when linking `libzml_cpu.so`), checked with `readelf -n`. Omn must not infer code identity from a pathname. Existing diff identity tests retain this invariant. |
| 6. Masked kernel addresses blamed on the wrong sysctl | The collection reports actual `kernel.kptr_restrict`, `kernel.perf_event_paranoid` and effective `CAP_SYSLOG`, naming a blocking control when these values explain it. At kptr=0/paranoid=2/no CAP_SYSLOG it names paranoid. Unknown controls and unexplained masking are stated rather than guessed. `stack::tests::masked_kernel_addresses_name_the_actual_controls`. No sysctl was changed; see the release-capability note below. |
| 7. Failed late attachment kills target and exits successfully | Failed attachments carry explicit collection failures through the normal command outcome (exit 1 for single and repeated traces). The driver waits for a running workload's natural exit or an explicitly requested duration, without a collector cadence causing a busy loop. A run in which no request attached also fails, including one whose workload ended before the first window opened. Driver tests exercise a sole failure and a real launched shell completing its output and exiting 7 without termination; runner tests exercise single/repeated empty measurements. |
| 8. Decimal durations rejected | The shared parser accepts `3.9s` exactly as `3900ms`, including windows. Integer decimal arithmetic preserves nanoseconds through `u64::MAX`; overflow, malformed literals and fractional nanoseconds are refused. `scalar::tests::decimal_durations_are_exact_and_never_rounded`. |

`runtime.func` is registered in the runtime catalog and collector dispatch;
it times named function entry/return per thread and recursion depth. The
selftest sweep uses the existing USDT fixture's named `ticks` function when
`--fixtures` is supplied, so it makes a valid function-filtered request. Its
scenario fixture checks sleeps (off-CPU time included), recursion, concurrent
threads, open calls, filters and probe cost. The user guide is
`docs/how-to-use/runtime.md`; the implementation's original brief is preserved
in `docs/dev/briefs/wave-29-func.md`.

## Late attachment and ring memory

The optional shared-ring/inheritance optimization is not applied to
`cpu.profile`: its task-local CPU clock deliberately preserves each task's
unfinished period across CPU migration. Sharing rings across tasks requires
per-CPU events; this loses unfinished periods on each CPU, the measurement
bias already documented in `src/collect/cpu/profile.rs`. The existing
`ThreadSampler` shares rings where per-CPU sampling is appropriate, but
substituting it here would change the profile's accuracy. Late attachment
therefore still needs sufficient perf ring memory for the thread count. A
memory-limit refusal now fails explicitly and lets the workload finish.

## Interpreter identity after local symbolization

Reading cached debug information names private interpreter functions that
previously appeared as module offsets. The latency verdict now uses the
sampled task's recorded executable before falling back to the launcher's
name. This keeps a Python process exec'd through `sh` recognizable as Python
when its stack labels no longer contain `libpython+offset`. A unit fixture
covers fully named Python frames and a separate native task.

## Validation on 27 October 2026

- `make CARGOFLAGS=--offline`: builds the workspace and grants the intended
  `cap_bpf,cap_perfmon` to `target/debug/omn`. Final debug binary SHA256:
  `aadc02ec8993f8779bf896d85440958c7d9e2ba601ccb23afb820b8d568234a9`.
- `cargo test --offline --test runner --test local_symbols --test scenario_func --test cli -- --nocapture`:
  75 tests passed (35 runner, 1 local-symbol test, 6 function-timing scenarios,
  33 CLI tests). The function-timing scenarios actually attached BPF probes;
  they did not take the unavailable-backend skip path. Runner coverage includes
  combined/separate logs, `/dev/null`, and exit 1 for single/repeated traces
  whose program exits before its decimal `3.9s..` window opens.
- `OMN_CASES_ONLY=cpython-interpreter cargo test --offline --test cases every_case_names_its_known_cause_both_ways -- --nocapture`:
  passed, after fixing interpreter recognition through recorded executables.
- `target/debug/omn selftest --format json --sources runtime.func --fixtures target --strict`:
  available, attached through `bpf:uprobe`, launch exit 0, using `function == 'ticks'`.
- `cargo clippy --workspace --all-targets --offline -- -D warnings`,
  `cargo fmt --all --check`, and `git diff --check`: passed.
- Final unit run: 1,900 passed, one ignored, three timing/accounting tests
  failed under concurrent load (`a_late_attach_that_fails_is_a_loss_and_the_run_goes_on`,
  `a_windowed_request_s_collection_states_its_own_window`, and
  `the_rollup_agrees_with_the_per_mapping_walk`). Each passed its one isolated
  rerun. A preceding full unit pass had 1,902 passing tests before the last
  additional regression test was added.

The complete `cargo test --workspace --offline --no-fail-fast` run finished,
but is **not green**: it reported six failing targets (`omn --lib`, `cases`,
`portability`, `reference`, `scenario_gpu`, `scenario_net`). Its CPython
regression was fixed and the actual case rerun successfully as noted above.
Other results are retained rather than hidden or relabeled as passes:

- The stored portability cells describe the previous 123-source catalog,
  without `runtime.func`; they require real resweeps against the 124-source
  catalog. An old Ubuntu 20.04 root cell also records five attachment
  violations. No stored matrix evidence was edited to manufacture coverage.
- The full cases run also failed GCC-shell classification and a Make/SQLite
  CPU-accounting tolerance. These cases have not been revalidated here.
- Reference comparisons failed in the bpftrace, GPU, I/O, perf-sched and
  sysstat families.
- One GPU scenario observed the same memory clock while idle and under load;
  network scenarios saw an empty connection collection and a capture ring
  dropping frames under load.

The complete run's log is `/tmp/omn-workspace-tests.log`; focused verification
is in `/tmp/omn-feedback-targeted-tests.log`, `/tmp/omn-cpython-case.log`,
`/tmp/omn-func-tests.log`, `/tmp/omn-func-selftest.json`, and
`/tmp/omn-clippy.log`. The matrix and unrelated comparison failures remain a
limit on broad portability/benchmark claims, not evidence that those gates
passed.

## Release binary

The optimized workspace has also been rebuilt with
`make release CARGOFLAGS='--release --offline' CAPS=cap_bpf,cap_perfmon,cap_syslog+ep`.
`target/release/omn` now has SHA256
`571b090eacbb91d80d8dd71206ba9518c08863cf928f73937bf601bfca634d14`.
A release smoke test passed for numbered combined logs and repeat artifacts,
`3.9s..` with repeated exit 1 and a naturally completed program, and
`runtime.func -e hist,summary` timing three sleeping calls. Its report is
`/tmp/omn-release-smoke.json`.

**Existing CAP_SYSLOG grant restored and verified.** The previous release
binary carried `cap_syslog,cap_perfmon,cap_bpf=ep`. Relinking removed its file
capabilities. `make caps PROFILE=release` restored BPF/perf; restoring CAP_SYSLOG
required the user's sudo authentication. That final restoration has now been
performed externally and verified with `getcap target/release/omn`:

```text
target/release/omn cap_syslog,cap_perfmon,cap_bpf=ep
```

The release SHA256 is unchanged, and the release smoke test was rerun
successfully after this verification. The capability restoration blocker is
resolved. No sysctl or sudo policy was modified, and no password was requested
in chat. The full-suite failures and optional shared-ring limitation described
above remain documented; they are not claimed as passing validations.
