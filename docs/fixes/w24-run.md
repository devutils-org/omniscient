# Stream run, the twenty-fourth wave: the run's shape

Branch `feat/w24-run`. Every item of `BRIEF.md`'s "run" section but one
landed: the machine's capacity in a recording lands in two files other
streams own, and is filed below. Two items moved a file outside this
stream's row. Both are said first.

## Outside the owned files, said first

- **`src/collect/mod.rs` (shared layer), one additive change**
  (`acf6142`). `run_core` takes `bound: Option<&[String]>`, a program it
  launches, ends with, and that no request targets. Two new entry points
  sit beside the old ones, which pass `None`: `run_all_bounded` and
  `record_bounded`. A bound beside a request whose target is a launch is
  `MixedTargets`. A `Target::Pid` workload with no launch is now watched
  through a pidfd each (`launch::Exits`). Its exit is a stop condition, so
  it ends the run, and `-d` is no longer needed. Nothing else in the runner
  moved. The `collect::` unit tests pass, 692 of 693. The one red,
  `mem::footprint::the_rollup_agrees_with_the_per_mapping_walk`, passed
  alone on its rerun.
- **`src/request.rs` (stream emit's), one field**: `Session::launch:
  Vec<String>` (`serde(default, skip_serializing_if = "Vec::is_empty")`),
  set in `Session::single`. The two `Session { .. }` literals in
  `src/triage/latency.rs` and `src/triage/saturate.rs` (stream triage's)
  gained `launch: Vec::new()`, and nothing else changed there.
- **`tests/view_model.rs` (stream views'), two lines**: the `Lifetime`
  literal gained `exit_code: None, signal: None`, or it would not compile.
- **New file `src/repeat.rs`**: a module of the binary (`mod repeat;` in
  `src/main.rs`), not of the library, so `src/lib.rs` is untouched.

## What landed

1. **A machine-wide target bounded by a launched program** (`acf6142`).
   A program beside `--system`, `--cpu`, `--cgroup`, `--iface`, `--pid`,
   `--tid` or `--container` is the window. That target is measured for as
   long as the program runs, the way `perf stat -a -- prog` and `turbostat
   prog` measure. A program alone is still the target. In a session file
   the window is `launch: [prog, args]` beside the requests, so
   `--dump-request` prints a runnable file. A request that launches a
   program of its own beside a `launch` is refused with exit 2 ("a run
   launches one program"). A source that cannot follow a program now gets
   a hint naming the bounded spelling (`431ade2`). The shared-stdout
   warning covers the bounded program too (`5836a6b`).
2. **Several sources on one launch.** `omn trace cpu.instructions,cpu.cycles
   -- prog` (commas or spaces) gives one request per source, with the same
   target and axes, over one window. A mode or emit the catalog leaves no
   choice about is defaulted per source. `--label` with several sources is
   refused, and so is a source named twice. `-e` stays the emit: perf's
   `-e a -e b` is spelled as a source list here.
3. **`-r N` / `--repeat N`.** Runs the session N times, each with a launch of
   its own. The result is each `summary` value's mean, sample standard
   deviation, min, max and perf's relative error of the mean
   (`rel_stderr_percent`). A launched program's `task:<pid>` keys become
   `task:#k`, ranked by pid within each run, so the runs line up. A
   structured format prints `{repeat, spread, runs: [RunOutput...]}`, and
   text prints one line per series mean. Under `-o`, each run is written to
   `run-<i>/` and the spread to `repeat.json`. Refused before anything
   runs: no `summary` to spread, `-r 0`, a recording, a binary format.
4. **Exit 1 says which.** The last line of stderr is one of three fixed
   sentences (`main.rs`, `Unavailable::line`):
   - `exit 1: the machine refused the run; nothing was measured`
   - `exit 1: the run failed after it started, before writing its results`
   - `exit 1: the run failed after it started; what it read is written above`
5. **`--pid` ends when its process exits**, through a pidfd, as `perf stat
   -p` ends. This holds with or without `-d`, and `-m count --pid` without
   `-d` now runs. With several pids the run ends when the last exits, and
   it notices at its own cadence: a pidfd that has fired stays readable,
   so the run cannot sleep on it. A tid is not watched, because a pidfd
   names a process.
6. **`omn trace --pid` says when it attached**:
   ``note: `cpu.profile` attached to pid N 1.52s after omn started; what it
   did before that is in no reading``. This is printed for every run on a
   pid, tid, cgroup or container target. Attaching within 500 ms is not
   done (see "Not done").
7. **The launched program's exit status in `results.json`**: `lifetime`
   gains `exit_code` (it exited) or `signal` (it was killed, `9` for a stop).
8. **The launched tree** (`8193889`):
   - The `PATH` search moved to the parent. The child makes one `execve`
     of the file the search found, and falls back to `/bin/sh` on
     `ENOEXEC`, as `execvp` does.
   - The exit is seen with `waitid(WNOWAIT)`, and the reap (`wait4`, which
     hands over the cpu) waits for `end_orphans`, after the collectors'
     closing round. The zombie's `/proc/<pid>/stat` is still there for
     that round.

## Shown vs reasoned

- **Bounded by a program, against perf** (shown). I ran `perf stat -a -e
  power/energy-pkg/ -x, -- omn trace power.energy --system -m count -e
  summary -- spin.py 2` three times, with perf wrapping omn. omn read
  243.02 J over 2.016 s, 247.79 J over 2.015 s and 231.31 J over 2.015 s,
  which is 120.6, 122.9 and 114.8 W. Perf read 248.98 J over 2.064 s,
  255.90 J over 2.081 s and 239.48 J over 2.085 s, which is 120.6, 123.0
  and 114.9 W. Each run's window was the program's 2 s, and every result
  said `stopped because the launched program exited`. The machine was
  loaded by the other streams.
- **`-r 5` against `perf stat -r 5`** (shown) on `env PYTHONHASHSEED=0
  python3 -S tests/reference/workloads/perfstat/spin.py 3000000`:
  `cpu.instructions` read mean 4 747 089 088, stddev 376 188, `+- 0.00 %`.
  `perf stat -r 5 -e instructions` read 4 747 921 521, `+- 0.01 %`, which
  is 0.018 % apart. A time-bounded spin under the wave's load spread
  8.8 % on omn's side and 8.4 % on perf's, which is the workload's spread,
  not either tool's.
- **`PATH` search** (shown after, reasoned before): `proc.syscall -m count
  --filter 'syscall == "execve"' -- python3 -c pass` counts 1 `execve` on
  this machine's 19-entry `PATH`. The before figure, 13 against strace's
  1, is `w22-perfsched.md`'s. I did not rebuild the old commit to repeat
  it.
- **The closing round before the reap** (shown, sysstat 9). I ran
  `cpu.oncpu_time -m poll@1s -e raw --backend force:fs:proc -- spin.py 2.5`
  three times. The polls read [0, 960, 920, 510] ms, [0, 990, 990, 510] ms
  and [0, 990, 990, 500] ms, with 0 failed reads. The sums were 2390,
  2490 and 2480 ms, against 2405.7, 2500.2 and 2485.6 ms from the reap.
  The 500 ms tail was the interval lost before.
- **`--pid` ends at the exit** (shown): `cpu.instructions -m count --pid
  <spin 1.5 s>` with no `-d` read `1 events over 1.33s, stopped because
  pid N exited`. With `-d 10s` it ended at the same exit. The count was
  15.7 G, and `perf stat -p` on another run read 14.6 G. Those are
  different attach moments, so it is not a pair.
- **Attach latency** (shown): `cpu.profile --pid` opened its window 1.01
  to 1.52 s after omn started, in the debug build. `cpu.instructions
  --pid` opened at 54 ms. Resolution alone takes 48 to 80 ms. The rest of
  `cpu.profile`'s time is `ProfileCollector::start` priming
  `/proc/kallsyms` (`stack::prime_kernel_symbols`) before the window opens.
- **Exit codes** (shown): `sh -c 'exit 3'` writes `"exit_code": 3`. A
  stopped `sleep 30` writes `"signal": 9, terminated: true`. A pid that
  does not exist ends stderr with the machine line.

## Tests

- `src/collect/launch.rs`: 3 new unit tests (13 in the module).
  `the_lifetime_carries_the_exit_code_or_the_signal`,
  `a_program_seen_exiting_is_readable_until_the_run_reaps_it`,
  `the_program_is_found_on_path_before_the_fork` (with a no-`#!` script
  through the shell).
- `src/repeat.rs`: `the_spread_is_the_mean_and_sample_deviation_across_runs`.
- `tests/cli.rs`: 5 new tests, 33 in the file.
  `a_machine_target_beside_a_program_is_measured_for_the_program_s_life`,
  `a_session_s_launch_is_written_down_and_a_second_program_is_refused`,
  `a_pid_target_ends_when_its_process_exits`,
  `a_repeated_run_reports_each_value_s_spread_across_the_runs`,
  `exit_one_says_whether_the_machine_refused_or_the_run_failed`.
- Also run: `cargo test --lib cli::` (12), `cargo test --bin omn` (9),
  `cargo test --test runner` (30), `cargo test --test view_model` (7),
  `cargo test --lib collect::` (692 of 693, the red one passed alone),
  `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt
  --check`. All green.

## Not done, and why

1. **The machine's capacity in a recording** (cpus, memory, so a replay
   colours by fullness). The writer is `src/main.rs`'s `omn top` (`Beside`
   in `view.json`), but the struct is `view/src/model.rs` (stream views'),
   and its reader `Capacity::of` is `view/src/views/process.rs` (stream
   top's). Filed below with the exact shape. Once the field exists, the
   `main.rs` half is three lines at the `Beside { .. }` literal, filling
   `available_parallelism()` and `MemTotal`.
2. **`omn trace --pid` attached in under 500 ms**: only half is done. The
   run now says when it attached. The 1 to 1.5 s is the kernel symbol
   table being read in `cpu/profile.rs`'s `start()`, before the window.
   That is stream cpu's file, and the table itself is stream unwind's
   `src/stack.rs`. Filed below.
3. **`omn` returning the launched program's exit code**, as `perf stat`
   does, was not asked. `omn` exits 0 when the run succeeded whatever the
   program returned, and the code is in `lifetime.exit_code`.
4. **A bounded target picked by default**: `omn trace cpu.util -- prog`
   is still refused, because `cpu.util` cannot follow a launch, and now
   comes with a hint naming `--system -- prog`. Defaulting to `--system`
   would turn the user's "this program" into "this machine" without being
   asked, and this project does not decide that on the user's behalf.

## Privileged runs wanted

None. Everything above ran with what `make` grants (`CAP_BPF`,
`CAP_PERFMON`), and the perf comparisons ran unprivileged
(`perf_event_paranoid` -1 here).

## Decision lines for `DECISIONS.md`

- **A program beside a target is the window, not a second target.** In a
  session it is `launch:`, beside the requests. The runner launches it,
  ends with it, and binds it to no request. A request that launches a
  program of its own beside it is refused: a run launches one program.
- **`--pid` ends with its process.** A pid workload's exit is a stop
  condition, like a launched program's. `-d` is a ceiling, not a
  requirement.
- **`-r N` spreads summaries only.** One number per series per run is what
  a repetition is read against. A request with no `summary` is refused,
  and every run's own results are kept beside the spread.
- **Exit 1's last line says which**: the machine refused, the run failed
  before writing, or it failed after writing. These are three fixed
  sentences a caller may match.
- **The launcher searches `PATH`, the child execs once**, and the exit is
  seen before it is reaped, so the closing round reads the zombie.

## Needs from another stream

- **views** (`view/src/model.rs`): a field on `Beside`, `#[serde(default,
  skip_serializing_if = "Option::is_none")] pub capacity:
  Option<MachineCapacity>`, with `MachineCapacity { cores: Option<f64>,
  memory_bytes: Option<f64> }`. Stream run (or the coordinator) then fills
  it in `src/main.rs`'s `Beside { .. }` literal. Also the view's
  `Lifetime` mirror, which does not carry the new `exit_code` and
  `signal`. `the_lifetime_mirror_keeps_every_field` passes only because
  its literal sets them to `None`.
- **top** (`view/src/views/process.rs`): `Capacity::of` reading
  `Beside::capacity` for a replay, in place of `Capacity::default()`.
- **cpu** (`src/collect/cpu/profile.rs`), with **unwind** (`src/stack.rs`):
  `start()` primes `/proc/kallsyms` before the window opens, which is 1.0
  to 1.5 s of a debug build before `omn trace cpu.profile --pid` attaches.
  Priming it on a thread at prepare, or at the first drain that meets a
  kernel frame, would bring the attach under 500 ms. The runner's own
  prime for late stacks (`collect/mod.rs`) has the same cost.
- **cpu** (`catalog/cpu.yaml`, `cpu.oncpu_time`'s fs:proc caveat): "a
  launched program is reaped before the run's closing read, so its cpu
  since the last poll ... is in no reading" is no longer true. The closing
  round now reads the zombie, and the measurement above shows the last
  interval kept with no failed read.
- **reference** (`tests/reference/runner.rs`): the runner can now read
  `lifetime.exit_code` and `signal` instead of a pidfd. It can tell a skip
  from a failure on exit 1 by the last line of stderr. A pair can now be
  written as `omn trace <src> --system -- workload` instead of the tool
  wrapping omn (perfstat 3, power 1, io 3, top 6, net).
- **lazy**: `TraceArgs::source` is a `Vec<SourceId>` now
  (`value_delimiter = ','`). The family-name dispatch lands where
  `cli::to_session` loops over `args.source` and calls `request_for`.
- **coordinator** (`man/omn.1`, `GUIDE.md`, `CATALOG.md`): document
  `-r/--repeat`, a program beside a target, a session's `launch:`, several
  sources, `--pid` ending at its exit, exit 1's three last lines, and
  `lifetime.exit_code`/`signal`.

## Review

Two findings, both fixed. Only `src/main.rs` and `src/repeat.rs` (this
stream's new file) moved.

- **Defect: `-r N` in text never said a run's losses.** Each run's
  `run i of N:` line on stderr is now followed by what that run lost, the
  way a single run says it: per request, `no loss: ...` or the `LOSS:` line
  with all five (six with eviction) counted kinds, each reason, and each
  degradation (`repeat::losses`). This holds in every format, since it is
  stderr. The text spread also ends with `LOSS in run 2 of 3: ...` when any
  run lost anything (`repeat::lossy`), so a mean with a lossy run in it does
  not read as clean. Shown: `omn trace cpu.instructions -m count -e summary
  -r 3 -- /bin/true` now prints `` `cpu.instructions`: no loss: every read
  succeeded and no tick ran late`` under each of the three run lines. The
  lossy path is shown by the unit test
  `repeat::tests::a_run_that_lost_says_so_and_the_spread_names_it`, not by a
  live run: no run on this machine was lossy on demand.
- **Gap: run k > 1 refused at collect said "the machine refused the run;
  nothing was measured".** Now it says `the k-1 run(s) before it measured`,
  plus `and wrote theirs to <dir>/run-<i>` under `-o`, and `no spread was
  taken`. The last line is `exit 1: the run failed after it started, before
  writing its results`, with exit 1 whichever gap the refusal names,
  because the session already ran. `exit_for(Whose::Machine)` also consults
  `RUN_STARTED` now, the same way `main`'s error path already did. A
  machine refusal after a run started is a failed run, so the three-sentence
  set stays closed. Reasoned, not shown: making a second run refuse after a
  first one succeeds needs the machine to change between the two runs.

Tests: `cargo test --bin omn` (10 of 10), `cargo test --test cli` (33 of
33; `a_refusal_for_an_unwritten_backend_leads_with_the_one_passed_over`
went red once, in the run right after `make` relinked, and passed alone on
its rerun), clippy and fmt clean.
