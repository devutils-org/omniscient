# The twentieth wave, stream budget

Branch `feat/w20-budget`, from `main` at `444900b`. `BRIEF.md`, "budget --
the budget and the run's account": every bullet of `STATE.md` item 7, the
first-check decision proposed with numbers, and `docs/mcp.md` following what
`mcp/` does now (item 10).

## What landed

Six commits, each with its tests.

- **`81f353b` -- the symbol table indexed, and kallsyms' usability decided
  once** (`src/stack.rs`).
  - A module is `mmap`ed rather than `std::fs::read`, and a `Function` is an
    offset, a size and where its name starts in the string table
    (`Elf::symbol_entries`). The name, and the prologue's frame-pointer
    verdict, are read the first time a frame lands in the function and kept
    (`Module::looked_at`). The debug file debuginfod gives is mapped the same
    way and kept beside the module. Pages a run never looks up are never read.
  - `KernelSymbols::usable` is decided at the read. `is_usable()` was a scan
    of every line, asked **per kernel frame**; whenever the answer is no --
    every run at `perf_event_paranoid` 2 without `CAP_SYSLOG`, where
    `/proc/kallsyms` reads as all zeroes -- that scan is the whole table each
    time. That is the "kernel frame named per sample" cost of item 7.
- **`4b7f258` -- the in-target meters read before the stops, and a one-time
  load weighed apart** (`src/collect/mod.rs`, `src/stack.rs`, `src/main.rs`).
  - `Spending::read_in_target_before_the_stops` reads every
    `InsideMeter` just before the final stop loop; the detach reads its
    meter before `collector.stop()` (as well as after), and `degrade` before
    `current.stop()`. A meter keeps its last reading, so a collector whose
    `stop` drops its programs no longer leaves a run without a budget at the
    zero of its first reading.
  - `stack::one_time_cpu_ns()`: the cpu this process spent loading tables
    it loads once (`Module::read`, `KernelSymbols::read`), on the loading
    thread's clock, process-wide. `Overhead::one_time_cpu_ns` is this
    collection's share of it over its span (all of it with one collector),
    outside `Overhead::percent`, `own_percent` and so the budget's checks
    (`running_cpu_ns`), and still in `collector_cpu_ns` and `tool_cpu_ns`.
    The cost line says `(X of it once, loading symbol tables, weighed apart
    from the percentage)`, and a stop's reason says it too. Serialized only
    when non-zero.
- **`02454bc` -- every request refused at resolution named** (`src/main.rs`).
  The trace path resolves every request before refusing: each error with its
  hint, then `error: N of the session's M requests cannot be resolved here`,
  and the exit code is the first gap to fix (`first_to_fix`: request, then
  build, then machine, as `CollectError::whose` orders a collect-time
  refusal of several).
- **`edd25ae` -- a stderr of the launched program's own, and the budget's
  stop in `run_session`** (`src/collect/launch.rs`, `mcp/**`).
  - `OMN_PROGRAM_STDERR` (`collect::PROGRAM_STDERR_VARIABLE`) names a file the
    launched program's stderr is written to: opened before the fork,
    close-on-exec in `omn`, `dup2`'d onto the child's fd 2 after the gate.
    The variable is not passed on to the program. Unset, nothing changes.
  - `run_session` sets it to `program.stderr` in the run's directory for a
    launching session and answers `program_stderr` (bytes, the last 64 KiB,
    the file) beside `program_stdout`; `said_by_omn` is `omn`'s lines alone.
  - `run_session` answers `stopped_by_budget`: `null`, or per request whose
    budget ended the run its label, source, budget and reason, from
    `cost.json`. `Binary::invoke_with` carries the environment.
  - `mcp/tests/walkthrough.rs` reads `program.py`'s `omn-truth` line from
    `program_stderr`; `protocol.rs`'s launch test reads `FDS 20` there.
- **`b2d0057` -- an orphan that left the group ended at a stop**
  (`src/collect/launch.rs`, `src/main.rs`). `collect::adopt_orphans()`, called
  once by the `omn` binary at start, makes it `PR_SET_CHILD_SUBREAPER`: a
  descendant orphaned before the stop is reparented to `omn`, not init.
  `Gated::terminate`, once the program is reaped, stops, kills and reaps
  every process `omn` adopted (`end_adopted`), counts them in
  `Lifetime::adopted_ended` (serialized when non-zero) and the run says
  `note: the stop also ended N process(es) the program started that had left
  its tree before it`. One it could not signal is counted in `left_running`.
  A library caller never adopts, so a test harness's other children are
  never touched.
- **`980190d` -- `docs/mcp.md`**: the budget optional (the walk-through, the
  decision's item 2 and the `DECISIONS.md` row); `program_stdout`,
  `program_stderr`, `bounds`, `budget_against`, `stopped_by_budget` in the
  answer's table; `said_by_omn` as `omn`'s alone; the protocol table with
  `a_launched_programs_stdout_comes_back_apart_from_omns_document` and the
  malformed budget; and two readings under "Naming what dominates": a
  followed task's table when the main thread waited on it, and `raw` beside
  `stack` to read one task's stacks (checked: a `cpu.profile` raw event
  carries `keys[].task.tid` and a `stack_id` attribute).

## Shown versus reasoned

Shown, on this machine (debug build, no capabilities on the worktree's
binary, `perf_event_paranoid` -1, nine agents sharing it):

- **The symbol load**: `stack::tests::the_cost_of_loading_one_symbol_table`
  (ignored; `OMN_SYMBOL_LOAD_FILE=<elf>`), three loads each, on the worktree's
  192 MB `target/debug/omn` (95 034 then 95 119 functions): **203 to 217 ms
  of cpu before, 79 to 81 ms after**. The rest is walking the symbol table
  and sorting it; the names and prologues are no longer touched up front.
- **The kallsyms scan**: `any(addr != 0)` over 421 479 zeroed entries (this
  machine's line count) costs **1.73 ms each in a debug test build** -- per
  kernel frame, before; one field read after. Measured with a scratch test,
  not kept. That the real file reads as zeroes at paranoid 2 without
  `CAP_SYSLOG` is the kernel's `kallsyms_show_value`, not reproduced here
  (it needs a sysctl).
- **The one-time load weighed apart**, `cpu.profile -e stack --budget 20%`
  over a python program that computes 20 000 additions and sleeps 20 ms, 40
  times (`-d 3s`), as `stopped` lines and cost lines:

  | `--symbols` | one-time load | first check, rate | the same with the load in |
  |---|---|---|---|
  | `local` | 4.8 to 5.0 ms | 46.65, 65.27, 310.44 % (stopped) | about 73, 82, 340 % |
  | `debuginfod` | 19.6 to 24.2 ms | 20.57 % at 0.85 s; 50.72, 44.69 % at 0.1 s (stopped) | about 39, 142, 152 % |

  And without a budget, the whole window: `local` 26.35 and 19.38 %,
  `debuginfod` 15.75 and 17.04 % with the load apart (3.6 to 5.4 ms and
  19.5 to 19.8 ms of it); with it in, `debuginfod` would read 38.5 and
  36.1 %. `a_one_time_load_is_weighed_apart_from_the_rate_and_still_spent`
  holds the arithmetic.
- **Every request refused named**: `examples/sched-forced-backends.yml` on
  the uncapable binary names all five sources, then `5 of the session's 5
  requests cannot be resolved here`, exit 1;
  `a_session_refused_at_resolution_names_every_request_it_could_not_resolve`
  (skips where the machine resolves them, which is what `make` will make it).
- **The program's stderr**: `omn ... -- sh -c 'echo hello-err >&2'` with the
  variable writes `hello-err` to the file and not to `omn`'s stderr;
  `a_program_given_a_stderr_of_its_own_writes_there_and_is_not_told_about_it`
  (the program sees no variable); the MCP launch, stdout and walk-through
  tests read it back through `run_session`.
- **The orphan**: `a_stop_ends_an_orphan_that_left_the_program_s_tree_and_group`
  -- `sh -c '(setsid sleep <marker> &) ; sleep 30'` under `-d 600ms`. Passes
  in 0.75 s after; **on the binary without `adopt_orphans()` it fails, and
  takes 71.7 s**: the orphan held `omn`'s stderr pipe open until its sleep
  ended, and the run never said it.
- **`stopped_by_budget`**: `a_run_the_budget_stopped_says_so_in_the_answer`
  on a written `cost.json`; null in the stdout test's real run. A real
  stopped run through `run_session` was not driven.

Reasoned, not shown:

- **The meters before the stops**: no test here drives a `stop()` that
  closes a program's descriptor with the kernel timing programs. The check
  that would show it is `budget::what_an_ebpf_program_spends_inside_the_target_is_measured_or_said_to_be_unknown`
  (`proc.syscall` drops its program in `stop`; no budget; `runs >= 2 * ops`)
  with capabilities and `kernel.bpf_stats_enabled=1` -- in "Privileged runs
  wanted". It took the unknown path here.
- **The one-time share with several collectors** is the one-time cpu times
  this collection's share of the process's cpu: right for one collector,
  an approximation for several (the load is paid inside one collector's
  drain, and the share spreads it by time spent inside collectors).
- **`end_adopted`** reaps with `waitpid(-1, WNOHANG)`, bounded at 200 ms; it
  assumes the binary has no child of its own at a stop other than the
  program and what it adopted. Checked by reading: every `Command` in the
  non-test code is `.output()` (waited for in place); `.spawn()` appears in
  tests only.

## What I could not do, and why

- **The first check is not settled by the one-time load alone.** With the
  load apart, the first check still reads 45 to 310 % where the whole window
  reads 16 to 26 %: the first 100 ms of a profile of a waiting program
  carry 8 to 80 ms of other first-drain work (maps, the unwinder's tables,
  the first names of each function) against the 18 to 25 ms the program
  burnt. The second half of the proposal (below) is a decision, not mine to
  land, and is not implemented.
- **What the remaining first-drain cost is** was not broken down: the
  310 % run (85 ms of the tool's cpu in 121 ms) is an outlier of three, on
  a shared machine.
- **`cpu.profile`'s own path** (`src/collect/cpu/profile.rs`, stream rings)
  was not touched: whatever it caches per sample beyond `Symbolizer` is its
  owner's.
- **The adopted orphans' cpu** is in no denominator until they are reaped:
  `task::tree_cpu` walks the program's tree, and an orphan has left it. As
  before this wave (they were init's), but now findable: the meter could
  add `omn`'s adopted children. Filed below rather than done, since the
  budget meter's semantics would move.

## Privileged runs wanted

On the worktree's binary after `make` (capabilities), alone on the machine:

```console
cd /home/moha/repos/omniscient-w20-budget && make
sudo -n /usr/bin/sysctl kernel.bpf_stats_enabled=1
cargo test --test budget what_an_ebpf_program_spends_inside_the_target -- --nocapture
sudo -n /usr/bin/sysctl kernel.bpf_stats_enabled=0
```

What it shows: `proc.syscall`'s programs, dropped in its `stop`, now read
before it -- `in_target_runs >= 2 * ops` without a budget, where the meter
would otherwise hold its first reading.

And the first check at paranoid 2, where kallsyms reads as zeroes (the
original 116 to 179 % case):

```console
sudo -n /usr/bin/sysctl kernel.perf_event_paranoid=2
./target/debug/omn trace cpu.profile -e stack --stack fp --symbols debuginfod --budget 20% -d 3s -- python3 -c 'import time
for i in range(40):
    s=0
    for j in range(20000): s+=j
    time.sleep(0.02)'
sudo -n /usr/bin/sysctl kernel.perf_event_paranoid=-1
```

## Decision lines for DECISIONS.md

- **What the budget's first check may stop a run on (proposed; half
  landed).** (1) *Landed*: a one-time cost the tool can name -- a symbol
  table loaded inside the window -- is weighed apart from the rate and said
  apart (`Overhead::one_time_cpu_ns`), outside the percentage and every
  check, inside what the tool spent. Measured: the first check of a
  `--symbols debuginfod` profile of a waiting program from about 142 to
  152 % down to 45 to 51 %, and its whole window from 36 to 39 % to 16 to
  17 %. (2) *Proposed*: **no stop while the collection is over its ceiling
  by less than 10 ms of the tool's cpu** (spent - ceiling x target <
  10 ms). Against the five runs the first check stopped here: the excess
  was 7.3 ms (`debuginfod`, 50.72 %: 12.0 ms spent, 0.2 x 23.7 ms allowed),
  5.4 ms (`local`, 46.65 %) and about 4 ms (`debuginfod`, 44.69 %) -- not
  stopped -- and 10.9 ms (`local`, 65.27 %) and 75 ms (`local`, 310.44 %)
  -- stopped. A collector over its ceiling at its running rate passes 10 ms
  within a second or two of a busy target and is stopped then. A slack in milliseconds rather than a floor on the
  target's cpu because a floor never stops anything on a program that waits
  99 % of the time, and the slack's cost is bounded: 10 ms of the tool's cpu
  over budget, once per run. Leaving (2) open costs every budgeted run of a
  waiting program at 99 Hz in a debug build its answer at the first check:
  5 runs of 6 here were stopped 0.1 s in, at 44.69 to 310.44 %, where the
  whole windows of the same program read 16 to 26 %.
- **A launched program's stderr can be its own** (`OMN_PROGRAM_STDERR`, a
  file), not passed on to the program; the MCP server always asks for it.
  A variable rather than a flag because `src/cli.rs` is not this stream's;
  a `--program-stderr FILE` flag is filed.
- **The `omn` binary is the subreaper of what it launches**, and a stop ends
  what it adopted; a library caller is not and does not.
- **A session refused at resolution names every request**, and its exit
  code is the first gap to fix (request, build, machine).

## Needs from another stream

- **To docs (`examples/mcp/approved/program.py`)**: its header says its
  `omn-truth` line reaches the agent in `said_by_omn`; it is in
  `program_stderr` now.
- **To docs (`tests/cases.rs`)**: `ask_mcp` reads `said_by_omn` for `omn`'s
  budget line (unchanged), but a case that read the program's own stderr
  there must read `program_stderr`; and (STATE item 10) its `MODEL_BUDGET`
  and the `budget is required` fallback can go.
- **To shared (`src/cli.rs`, `man/omn.1`)**: a `--program-stderr FILE` flag
  setting what `OMN_PROGRAM_STDERR` sets, and both in the man page, with
  `Lifetime::adopted_ended` and the subreaper in the stop's paragraph.
- **To view (`view/src/model.rs`)**: `Lifetime::adopted_ended` and
  `Overhead::one_time_cpu_ns` are new optional fields of `results.json`, if
  the mirror holds those structs field by field.
- **To the coordinator (`src/collect/task.rs` is mine, but the meter's
  meaning is a decision)**: whether a launched program's denominator takes in
  `omn`'s adopted children (orphans of the program) while they live.

## The suite

Run, each passing: `cargo test --lib stack` (68, 1 ignored),
`cargo test --lib collect::` (663: one failure,
`collect::tests::the_runner_closes_the_window`, a 20 ms tick under load,
passing alone once), `cargo test --lib collect::launch` (9),
`cargo test --test budget` (25), `cargo test -p omn-mcp` (11 unit, 12
protocol, 2 walk-through), `cargo test --test unwind` (13),
`cargo test --test stack_capture` (17), `cargo test --test cli` (26), then
`cargo clippy --workspace --all-targets -- -D warnings` and
`cargo fmt --check`, both clean. No `make`, no capabilities on this
worktree's binary: the eBPF halves skipped, loudly.
