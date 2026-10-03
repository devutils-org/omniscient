# Stream build, seventeenth wave: durability, a compiler and a build

ROADMAP phase 12, cases 2, 5 and 6. Everything here is in
`cases/sqlite3-durability/`, `cases/held/gcc-shell/` (held back, section 10),
`cases/make-sqlite/` and `cases/shared/watch.py`; outside `cases/`, only this
file and the CI's workload step (section 10) changed.

---

## 1. What landed

- **Case 2, `cases/sqlite3-durability/`**: the distribution's stripped
  `sqlite3` reading a file of single-row INSERTs with `.read`, each its own
  commit, in a database under `{cache}/work/<the run's scratch directory>/`,
  linked from `{dir}/work` (a need refuses a `{cache}` on tmpfs, where a sync
  returns at once). Every run -- each way, each reference -- starts from a
  fresh copy of an empty database.
  - Off the cpu, `every-commit-waits-for-the-disk`: a rollback journal with
    `synchronous=FULL`, 500 commits, four `fdatasync` a commit. Expected:
    `blocked`, limit `disk`, leaf `fdatasync`, through
    `sqlite3PagerCommitPhaseOne` (by the debug file debuginfod serves for this
    build id, the dominant stack is `fdatasync <- syncJournal <-
    sqlite3PagerCommitPhaseOne <- sqlite3BtreeCommitPhaseOne`).
  - On it, `commits-that-wait-for-nothing`: a write-ahead log with
    `synchronous=NORMAL`, 50 000 commits of 1000-character rows, 0.005 syncs
    a commit (the checkpoints'). Expected: `on-cpu`, limit `cpu`.
  - References: `.timer` (`sqlite3-timer`, from the target); `fsyncs.py`, a
    second run of the same commits under `strace -f --seccomp-bpf -c`,
    counting `fsync` and `fdatasync` (the measured run is never traced);
    `watch.py`, the program's own `/proc/<pid>/task/*/schedstat` read from
    outside once a millisecond; and `bare`, the same 50 000 commits with no
    tool attached, printed beside the measured run and asserted on nothing.
- **Case 5, `cases/held/gcc-shell/`, held back** (section 10): the
  distribution's gcc 16 compiling sqlite's
  `shell.c` (1.2 MB, 155 headers) from the amalgamation, fetched once into
  `{cache}` and sha256-checked.
  - On the cpu, `the-passes-o2-adds`: two builds, `-O2` asked and `-O0` run
    by the reference; the reference is gcc's own `-ftime-report` of both
    (`ftime.py`: `TOTAL`, `phase opt and generate`, `phase parsing`, their
    ratio). Measured: 7.0 s against 0.85 s, ratio 8.3.
  - Off it, `a-cpu-shared-with-two-competitors`: two processes spinning on
    cpu 0, the compile launched on cpu 0 (`taskset -c 0`, an affinity given at
    exec); the reference is `watch.py`'s `schedstat` of every process under
    the driver, the whole chain's wait for a cpu (`run_delay_ns`: the
    driver, cc1 and `as`). Measured: cc1 ran 1.05 s and waited 2.10 s for
    its cpu; the chain waited 2.24 s.
- **Case 6, `cases/make-sqlite/`**: GNU make 4.4 on sqlite 3.53.4's autoconf
  release, fetched once, configured and built once in setup, then rebuilt
  from its one source at every run (`-W sqlite3.c`, `CFLAGS=-O0`: the
  release's `-O2 -g` takes 45 s a build here, `-O0` 5 to 7 s, the same shape).
  - On the cpu, `the-critical-path-is-one-job`: of four jobs, the shell's
    (`shell.c` then `sqlite3.c` in one driver) lasts 99.9 % of the build.
  - Off it, `a-cpu-shared-with-two-competitors`: the library's object built
    on cpu 0 beside two spinners: its job ran 4.4 s and waited 8.8 s.
  - The reference of both is `watch.py`: per job (a process make started,
    with everything under it), its life, its subtree's cpu and wait for a
    cpu, the longest job's share of the build.
- **The helpers print `key=value` lines** read by the existing `key-value`
  extractor, so no extractor was added to `tests/cases.rs` (which this stream
  does not own). `watch.py` is one file, `cases/shared/watch.py`, which the
  cases call as `{case}/../shared/watch.py` (`cases/shared/` has no
  `case.yml`, so it is not a case). Before it watches, it ends every process
  an earlier run of the case left (section 10).

## 2. Shown, and reasoned

**Shown** on the development machine (24 cpus, kernel 7.2.7,
`perf_event_paranoid` -1, `omn` with `cap_bpf,cap_perfmon` from `make`, five
other agents running), `cargo test --test cases` three times, the last one
with every line in its final state (2 min 43 s for the four cases):

```
sqlite3-durability/every-commit-waits-for-the-disk/cli: blocked 1677.7 ms (disk, fdatasync), on-cpu 475.4, runnable 35.0
    kernel: blocked 1719.1 ms, cpu 443.2, run delay 14.3; .timer cpu 434.9 of real 2170.4; 4 syncs a commit
sqlite3-durability/commits-that-wait-for-nothing/cli:  on-cpu 1817.5 ms (cpu); .timer cpu 1632.7 ms of real 1752.9; bare .timer cpu 903.7 ms
gcc-shell/the-passes-o2-adds/cli:                      blocked 7123.2 ms (sleep, wait4); cc1's -ftime-report 7020 ms, -O0 850 ms
gcc-shell/the-passes-o2-adds/mcp:                      on-cpu 4771.3 ms, leaf [vdso]+0xd4c (cc1's table)
gcc-shell/a-cpu-shared-with-two-competitors/cli:       blocked 3511.3 ms (sleep, wait4); cc1 cpu 1046 ms, waited 2102 ms
make-sqlite/the-critical-path-is-one-job/cli:          blocked 10697.9 ms (sleep, wait4 <- update_goal_chain); longest job cpu 7798 ms, 99.9 % of the build
make-sqlite/a-cpu-shared-with-two-competitors/cli:     blocked 13203.5 ms (sleep, wait4); the job's cpu 4373 ms, waited 8783 ms
```

- **Case 2 is the method working on a program nobody wrote for it**, both
  causes, both ways: the first suspect is the known cause, the leaf is the
  sync, and both ways name the same suspect. Its findings are the numbers
  (section 3, "a blocked row called exact is short of the kernel's") and the
  two names findings case 1 already filed.
- **Cases 5 and 6 are the confident wrong suspect phase 12 was written
  against**, every cause, the person's way and (all but one run) the agent's:
  the verdict decomposes the launched process's main thread, which spends
  the run in `wait4`, and says it was *"asleep on something that is not a
  lock: a timer, a pipe or a socket"*. The compiler and the jobs that did the
  work are in the run (`also: task:N: largest row on-cpu at 6.656s`) and in
  no answer. Section 3, "a program is its main thread".
- **What the `-ftime-report` and `schedstat` references establish**: the
  causes were there, by the program's or the kernel's account (each `holds`):
  `-O2` is 8 times `-O0`; the longest job is 99.9 % of the build and waited
  0.1 % of its life; under contention cc1 and the library's job waited 67 %
  of theirs.
- **The tool's cost on these programs, shown**: the run's own account of its
  cpu under `omn latency` is about twice the bare run's (section 3, "the
  triage's mutex shim doubles a real program's cpu"), and cc1's profile has
  18 % of its samples in `libomn_preload.so`.
- **Reasoned, not run**: `perf_event_paranoid` 2 (privileged, section 6).
  Each cause there either runs as here (with the capabilities) or skips both
  ways with *"this machine withholds a source it needs: error: ..."*, as case
  1's do; nothing in these cases adds a source. Where `strace`, `unzip`,
  `taskset` or `curl` is missing, the case skips with its `needs` line (or the
  fetch's error) first.

## 3. Findings the cases show, filed

Each is a line under its case's `open_findings`, printed `FINDING` while it
fails. Those already filed by stream cases are cited, not repeated.

### "a program is its main thread, and the compiler driver's waits for its children" -- `src/triage/latency.rs` (task choice, `Limit::Sleep`), for the coordinator

`omn latency` decomposes the launched process's main thread (*"A program's
latency is its main thread's life; the other threads are listed"*). For a
compiler driver and for make, that thread is in `wait4` for the whole run:
`blocked: sleeping 7.242s (99.9 %)`, leaf `wait4 <- gcc+0xb5994`, while
`also: task:1145924: largest row on-cpu at 6.656s` is cc1 doing the work.
make: `wait4 <- update_goal_chain <- main`. Every cause of cases 5 and 6 gets
the same first suspect whatever was put in front of the program -- the
optimisation passes, a cpu shared with two competitors, the critical path.

Two things make it worse than an incomplete answer:

- **A child's table cannot be read even where it is listed.** `cpu.profile`
  follows the target's children; `sched.runqlat` and `sched.offcpu` do not.
  The contended compile through `omn -f` on the dumped session: cc1's table
  is `on-cpu 1018.1 ms, runnable 0.0, blocked 0.0`, residue 2416 ms, where the
  kernel says cc1 waited 2059 ms for its cpu. Descending into it today would
  name cc1 confidently on the cpu.
- **The sentence calls a wait for a child "a timer, a pipe or a socket"**
  (`Limit::Sleep`), the same sentence case 1 filed for a polled lock.

Ask: when the dominant block of the main thread is a wait for its own child
(`wait4`, `waitid`, a `pidfd`), follow the critical path into that child --
the child that was running during the wait -- and decompose it; the sched
sources follow the target's descendants as `cpu.profile` already does; and
the sentence names a wait for a child as one. Checks:
`the-first-suspect-is-the-known-cause`, `every-cited-number-agrees-with-the-reference`,
`the-sentence-claims-nothing-false` in `gcc-shell` and `make-sqlite`.

### "a budget weighs the launched process alone" -- `src/main.rs`, `src/collect/mod.rs` (the budget), the launcher; for the coordinator

`run_session` requires a budget (case 1's finding), and the budget weighs the
collectors' cpu against the *launched process's* cpu: *"`cpu.profile` spent
8581.01 % of the target's cpu time"* for a gcc driver that used 4 ms while
cc1, which `cpu.profile` was also sampling, used seconds. Every cause of
cases 5 and 6 is stopped at `100%`. And **the stop kills the main thread
only**: after `omn -f <the -O2 session> --budget 100%` returned, cc1 was
still running, reparented to the user's subreaper, three seconds later
(`ps -C cc1`: `ppid 1523`, `etime 00:00`, `00:01`, `00:02`).

The second effect reaches the cases: the harness asks a stopped run again,
unbounded, and whatever the stopped run had started is still running --
pinned to the contended cpu by `taskset -c 0` it inherited, so a fourth
competitor the reference does not count; in make-sqlite, still writing
`sqlite3.o` while the retry builds and links the same object; in gcc-shell,
still writing its `-ftime-report` into the `o2.report` the retry truncates,
where a stale `TOTAL` line can survive and `ftime.py` reads the last one.
The cases now end those processes themselves before each run (section 10);
the harness should too, for a case without a watcher. On the passes, the
agent's answer was the driver's `wait4` when the first run held its budget
and cc1's on-cpu (`[vdso]+0xd4c`, its table missing both sched rows:
*"sched.runqlat lost events, so its absence is not a zero"*) when it had
been asked again: 3 runs of 4, the last one after the retry had ended every
leftover (none was found: the stop had come before cc1 started), so the
leftover cc1 is not what made the agent name cc1 -- the agent reads the
heaviest table, and the driver's `blocked` and cc1's `on-cpu` weigh about
the same (section 3, "not filed as a check"). Ask: the budget's denominator
includes the cpu of the target's descendants that the collectors watched; a
stop ends the target's process group (or every descendant it started), not
its main thread. Checks: `the-agent-s-way-answers-within-a-model-s-budget`
(cases 5 and 6), `both-ways-name-the-same-suspect` (case 5, held back for
it, section 10).

### "the triage's mutex shim doubles a real program's cpu" -- `src/triage/latency.rs` (`OPTIONAL`), `shim/src/lib.rs:384`; for the coordinator

Case 2's 50 000 WAL commits, by sqlite's own `.timer`, through the triage's
dumped session (`omn -f`, each variant three requests or fewer):

| Run | `.timer` cpu |
|---|---|
| bare | 0.94 s |
| the triage's five requests | 1.86 s |
| the same without `sched.mutex` | 1.03 s |
| `sched.mutex` alone | 1.71 s |

The thread-safe sqlite3 takes a pthread mutex on every API call, so the shim
reports a record per lock, and still drops 179 311 of them. The verdict says
nothing of the dilation: the `on-cpu` row agrees with `.timer`, because
`.timer` measures the dilated run. Ask: the triage asks `sched.mutex` only
when the run shows contention to explain (it is in `OPTIONAL` already), or
says what the shim cost the target, which the ring's account has.

**A second, smaller cost in the same file**: `Timer::start`
(`shim/src/lib.rs:384`) reads `CLOCK_MONOTONIC` before it checks whether the
call reports at all, and only a reporting call counts down `UNTIL_TIMED`, so
**every** call of a class that does not report reads the clock. With the
shim preloaded and nothing configured, 20 M `malloc`/`free` pairs take 4.4 s
against 0.12 s bare with the debug shim the tests use, 1.07 s with the
release one; `perf` puts 37 % of that run in `__vdso_clock_gettime`. Under
the triage, cc1 had 825 of its 4588 samples (18 %) in `libomn_preload.so`,
and the agent's hottest function for cc1 was `[vdso]+0xd4c`: the tool's own
clock, named as the compiler's hot spot. Ask: decide `reporting(class)`
before the clock read (or count down on every call).

### "a blocked row called exact is short of the kernel's" -- `src/collect/sched/offcpu*`, `src/collect/sched/runqlat*` (and their eBPF programs); for the coordinator

Case 2's 500 commits (about 11 500 waits), both ways, three runs: the
decomposition's `blocked` row, which it calls exact, is 37 to 60 ms (2 to
3.5 %) under the program's own `schedstat` (life less cpu less run delay),
and its `runnable` row twice the kernel's run delay (35.0 ms against 14.3,
33.0 against 15.2). sqlite's `.timer` agrees with the kernel: the tool's
`blocked` plus `runnable` over the whole life (1712.7 ms) is less than
`.timer`'s off-cpu over the statements alone (1735.5 ms). About 1.7 us a wait
leaves `blocked` for `runnable` (and the on-cpu row is 30 ms over the
kernel's cpu, the profile's interrupt caveat). Ask: find where a wait's
`queued_ns` starts and ends against the kernel's `sched_info` run delay, and
either align them or give the rows the resolution the bias implies. Check:
`every-cited-number-agrees-with-the-reference` in `sqlite3-durability`
(`blocked` and `runnable` against `watch.target_*`, slack the watcher's
poll and 5 ms for the gate and reap edges).

### Already filed, shown again

- **"an off-cpu stack cannot ask debuginfod"** (`docs/fixes/w17-cases.md`):
  the commit under the sync is `sqlite3+0x19a6a9 <- sqlite3+0x6eabb`, both
  ways. `the-block-s-frames-are-named` in `sqlite3-durability`.
- **"the verdict drops where its names came from"**: every case.
- **"a waiting program exceeds every budget"**: cases 5 and 6, compounded
  by "a budget weighs the launched process alone".

### Not filed as a check

- **The harness reads a multi-task decomposition by its heaviest table**
  (`tests/cases.rs`, `read_mcp`, `tables.first()`, as `docs/mcp.md` says).
  Tables are ordered by the sum of their rows (`src/join.rs:3485`), so the
  agent's answer is whichever task's rows add up to more, which for a driver
  and its compiler is close to a coin. Worth a sentence in `docs/mcp.md`
  once the method answers for the critical path. For stream cases and the
  coordinator.
- **A child's `whole` is the launched program's life**: cc1's table above
  has `whole 3434 ms` (the driver's) against cc1's life of 3093, so its
  residue holds the life it did not have. Part of the first finding.

## 4. What I could not do, and why

- **The brief's off-cpu causes for gcc and make.** "Headers on a cold cache"
  for gcc was measured and does not dominate on this machine's NVMe:
  dropping cc1, the driver and the 155 headers with `POSIX_FADV_DONTNEED`
  adds 40 to 45 ms to a 0.86 s `-O0` compile, 45 ms to a 0.11 s
  `-fsyntax-only`, 35 ms to a 0.05 s `-E` -- never the first suspect, so it is
  not a cause a case can hold the triage to. "Many short processes" for make
  is the sqlite build's `configure` (about a hundred probes in 2 s), which is
  not make; the build itself has four jobs. Both cases use a cpu shared with
  competitors instead, the draft's "cpu contention" injection, and say why in
  their `about`.
- **A deterministic `both-ways-name-the-same-suspect` in `gcc-shell`.** On
  the passes the agent's way names whichever of two tables weighs more, and
  they weigh about the same; nothing in the case decides it. So the case is
  held back (section 10), not merged as a coin.
- **Two references the brief named.** Case 2's `/proc/<pid>/io`: its
  counters are bytes and calls (`wchar`, `syscw`, `write_bytes`), and the
  verdict cites time; nothing it holds is compared with a number the tool
  says, and what it would establish -- that each commit reaches the device --
  is said more directly by `fsyncs.py`'s count of the syncs themselves (four
  a commit) beside the kernel's own split of the program's life
  (`schedstat`, `target_blocked_ns`). Case 6's `make --trace` timings: GNU
  make's `--trace` prints why each target is remade and the recipe, with no
  time at all (make 4.4 has no timing output), so the timings are the
  kernel's, per job, from `watch.py`, which is the account the critical
  path needs (each job's life, cpu and wait for a cpu).
- **Extractors in `tests/cases.rs`.** Not this stream's file. The new
  accounts are read by scripts in each case's directory that print
  `key=value`; the same parsers as registered extractors are a request below.
- **`docs/cases.md`'s table of cases** and its injection table (the
  `schedstat` watcher, `taskset` plus competitors) are stream cases' file: a
  request below.
- **`perf_event_paranoid` 2**: machine-wide; section 6.

## 5. Tests

- While iterating: `cargo test --test cases` only (four runs, 0.8 to 4.5
  min, the last 2 min 43 s), and `cargo test --test cases
  every_case_is_well_formed` after each edit of a `case.yml`.
- **The cases test went from about 15 s to about 2 min 45 s.** The gcc
  `-O2` compile (7 s under the tool) and the contended builds (3.5 s and
  13 s) are each run three times a cause (the person's way, the agent's, and
  the agent's again after the budget stopped it). The budget fix in section 3
  takes a third of it back.
- **The review round**: `cargo test --test cases` three times with every
  change in (the first still with gcc-shell in `cases/`): 155 s, then 114 s
  and 114 s with it held back, all three green; the WAL cause's agent budget
  held every time, every watcher's `swept=0`, and after the third only that
  run's directory was left under `{cache}/work/`.
- Before the last commit, one `make check`: result in section 9.

## 6. Privileged runs wanted (the coordinator, alone)

```console
cd /home/moha/repos/omniscient-w17-build && make
# the CI's defaults cell, with the capabilities: every cause as here, no skip expected
sudo -n /usr/bin/sysctl kernel.perf_event_paranoid=2
cargo test --test cases -- --nocapture 2>&1 | grep -E '^case |^  |SKIPPED|test result'
# without them: every cause of the four cases skips both ways with the machine's reason first
touch src/main.rs && cargo build --workspace && getcap target/debug/omn   # (empty)
cargo test --test cases -- --nocapture 2>&1 | tee /tmp/cases-unprivileged.log | grep -E 'SKIPPED|test result'
.github/scripts/skip-gate.sh /tmp/cases-unprivileged.log .github/expected-skips.txt
sudo -n /usr/bin/sysctl kernel.perf_event_paranoid=-1 && make
```

Nothing in these cases needs `null_blk`, `dmsetup` or a sysctl of its own.

## 7. Decision lines for `DECISIONS.md`

| Question | Decision | Why |
|---|---|---|
| A case's account of a new shape, from a stream that does not own the registry | **A script in the case's directory that prints `key=value`, read by the `key-value` extractor**; promoted to a registered extractor by the registry's owner when a second case needs it. | A case stays a directory; the parser sits beside the reference it reads, as `holder.py` does. |
| The kernel's account of a task's time | **`/proc/<pid>/task/*/schedstat` of the target and every descendant, read from outside once a millisecond (`watch.py`)**, with the longest gap between reads as the resolution. | It is the kernel's own split of on a cpu, waiting for one, and the rest; reading it changes nothing in the target, and it splits what `.timer`'s off-cpu cannot (runnable from blocked). |
| Cpu contention without touching the target | **Competitors pinned to cpu 0, the target launched with `taskset -c 0`.** | An affinity given at exec is the target's environment, like a cgroup; without it the scheduler moves the target away from any one contended cpu. |
| A durability case's storage | **The case's `{cache}`, checked not to be tmpfs**, never the temporary directory. | A sync on tmpfs returns at once; the cause is a sync that waits for a device. |
| A cause the brief named that does not dominate | **Measured, reported with its numbers, and replaced by one that does** (gcc's cold headers: 45 ms of 0.86 s). | A case holds the triage to its first suspect; a cause that is not the first suspect by the program's own account is not a case. |

## 8. Needs from another stream

- **coordinator** (`src/triage/latency.rs`): the critical path through a
  wait for a child; the sentence for such a wait; `sched.mutex` asked only
  when needed or its cost said ("a program is its main thread", "the
  triage's mutex shim doubles a real program's cpu"). Each removes lines
  from `cases/make-sqlite/case.yml` (the test fails until they are removed)
  and from `cases/held/gcc-shell/case.yml`, which goes back to
  `cases/gcc-shell/` with the first of them (section 10).
- **coordinator** (the sched collectors, `src/collect/sched/`, their eBPF
  programs): follow the target's descendants as `cpu.profile` does; the
  per-wait bias between `blocked` and `runnable` ("a blocked row called exact
  is short of the kernel's"; removes `every-cited-number-agrees-with-the-reference`
  from `cases/sqlite3-durability/case.yml`).
- **coordinator** (`src/main.rs`, `src/collect/mod.rs`, the launcher): the
  budget weighs the descendants the collectors watched; a stop ends the
  process group ("a budget weighs the launched process alone").
- **coordinator** (`shim/src/lib.rs:384`): no clock read on a call that does
  not report.
- **cases** (`tests/cases.rs`): after a budget stop, wait for (or kill) the
  stopped run's descendants before asking again. The cases that call
  `watch.py` now do it themselves; a case without a watcher would not.
  Optionally, the three accounts as registered extractors
  (`gcc-time-report`, `strace-summary`, `schedstat-tree`) if the owner
  prefers them to `key-value` scripts; and a way to hold a case back that the
  well-formedness test still reads (a `held:` field), so
  `cases/held/gcc-shell/` is checked while it waits.
- **cases** (`docs/cases.md`): cases 2 and 6 in "The cases" table (case 5
  as held back), the `schedstat` watcher and `taskset` plus competitors in
  the injection table, and `cases/shared/` as the home of a helper more than
  one case calls.
- **coordinator**, for the merge: this branch adds `sqlite3`, `strace` and
  `unzip` to the CI's workload step (`.github/workflows/ci.yml`; gcc and make
  are build-essential's, `taskset` util-linux's, all three installed
  already). It has to land with the cases or before them: a case whose
  `needs` fails prints `SKIPPED case ...: this machine cannot run ...`, which
  no pattern in `.github/expected-skips.txt` matches, and the skip gate goes
  red. If the merge takes the cases without the workflow change, it takes
  that red too.

## 9. The last `make check`

- **After the review round** (`1b8e303`): `make check` green, 2189 passed,
  0 failed, 10 ignored over 51 test binaries, clippy and fmt clean, `omn`
  carrying `cap_bpf,cap_perfmon` throughout; `cases` 4 passed in 117 s. The
  unstable WAL budget line below is gone (section 10), and gcc-shell's is
  held back with its case.

The round before it, for the record:

- **`make check`** (after the cases' commit, `066332c`): `cases` 4 passed
  (160 s); red on one test outside this stream's files,
  `cross_machine::a_memory_limit_turns_a_cached_read_into_device_reads_and_each_byte_carries_its_rung`
  (*"64 of the 492 credited lie in intervals the program did not overlap"*).
- **Then one `cargo test --workspace --no-fail-fast`**: 2186 passed, 3
  failed, 10 ignored.
  - `cross_compute::a_lossy_profile_marks_its_row_and_leaves_the_traced_rows_alone`
    and `recorder::two_bursts_write_exactly_two_documents_each_holding_its_burst`
    (the latter in stream cases' tally), and the `cross_machine` test above:
    each rerun alone once, each passed. Load; this branch changes nothing
    they read.
  - **`cases`, mine**: `sqlite3-durability/commits-that-wait-for-nothing/mcp:
    the-agent-s-way-answers-within-a-model-s-budget` failed: *"[sched.mutex]
    --budget 100% was exceeded: 103.77 %"*. It had passed in the four runs
    before. It is the mutex shim finding (section 3) reaching the budget at
    the edge: the shim spent as much cpu inside sqlite as sqlite spent
    itself. Not listed under `open_findings`, since it passes most runs and a
    listed check that passes everywhere fails the test; it is a second
    unstable line beside `gcc-shell`'s, for the coordinator to weigh at the
    merge. The `sched.mutex` fix in section 3 removes it.
- Clippy and fmt: no Rust changed on this branch.

## 10. Review

The adversarial review's findings, and what became of each.

- **Defect: two nondeterministic lines in `cargo test --test cases`.**
  - *gcc-shell's `both-ways-name-the-same-suspect`*: **held back**. The case
    moved to `cases/held/gcc-shell/`, which the harness does not discover
    (it reads `cases/<name>/case.yml`), with a header saying why and when it
    returns. Making the line deterministic from the case was tried first:
    the watcher now ends every process an earlier run left (below), and in
    the run after that change the agent's retry found nothing to end
    (`watch.swept=0`) and still named cc1's `on-cpu` (5093 ms against the
    driver's `blocked` 6836 ms). The agent reads the heaviest table, and the
    two weigh about the same: nothing the case controls decides it. It
    returns with `git mv cases/held/gcc-shell cases/gcc-shell` once the
    triage follows a wait for a child (the agent then reads the critical
    path) or the budget stops weighing the driver alone.
  - *sqlite3-durability's agent budget on the WAL cause*: **made
    deterministic**. The ratio the budget checks is the mutex collector's
    cpu over the program's, and both scale with the commits, so it is a
    property of a commit's shape: with 64-character rows, 92 to 99 %
    (`omn -f <the dumped session> --budget 100%`, four runs), once 103.77 %.
    With 1000-character rows a commit does more work for the same locks:
    74.2, 73.9, 73.9, 75.7 % (2000 characters: 62 to 67 %, but the
    checkpoints' syncs reach 10 % of the life). The cause still waits for
    nothing (0.0046 syncs a commit) and is still on the cpu; the 1000 are in
    the cause's `about`, with the 64-character figures. The dilation itself
    stays filed ("the triage's mutex shim doubles a real program's cpu") and
    printed by the `bare` reference.
- **Defect: case 2's databases in shared, writable state.** Now
  `{cache}/work/<the run's scratch directory's name>/`, which holds the
  test's pid, linked from `{dir}/work`; every command and the watcher's
  marker name `{dir}/work/...`, so two checkouts never share a file or a
  watcher's target. Setup removes a directory whose run's scratch directory
  is gone (an earlier run's); a run's own stays until the next run, since a
  case has no teardown.
- **Gap: gcc's off-cpu ground truth was cc1 alone.** The `runnable` row is
  now compared with `watch.run_delay_ns`, every process under the driver and
  the driver itself (2.24 s in the run above, against cc1's 2.01 s): the
  critical path of a driver that runs one child at a time. `watch.py`'s
  docstring says so. `longest_waited_share` stays as the hold (cc1 waited
  more than it ran).
- **Gap: references the brief named and the handoff did not explain.**
  Section 4, "two references the brief named": `/proc/<pid>/io` holds bytes
  and calls where the verdict cites time, and `make --trace` prints no
  timings at all (checked: `update target 'a' due to: target does not
  exist`, then the recipe).
- **Gap: the retry after a budget stop runs beside the stopped run.**
  Section 3 now lists everything it reaches (the inherited `taskset`, the
  object make-sqlite rebuilds, gcc's report file). And `watch.py`, before it
  watches, ends every process whose command line names the case's scratch
  directory or whose working directory is under it, other than itself and
  its ancestors, and waits for them to be gone (`swept=N` in its account).
  Every cause of make-sqlite and case 5 starts it (case 5's passes now have
  the watcher as their injection, which also gives them `target_seen`); the
  harness-side fix stays a request to stream cases (section 8), for a case
  without a watcher.
- **Gap: the CI request missed `sqlite3`.** This branch now adds `sqlite3`,
  `strace` and `unzip` to the CI's workload step itself, so the merge brings
  the packages with the cases; section 8 says why they must land together.
- **Nit: case 2's database grew across the run.** Every run, each way and
  each reference (`bare`, `fsyncs.py`), starts from a fresh copy of an empty
  database: the target is `sh -c 'cd {dir}/work && rm -f ... && cp
  wal.empty.db wal.db && exec "$@"' sh sqlite3 ...`, the `exec` keeping the
  pid, so the watcher reads sqlite3's `schedstat` alone; `fsyncs.py` takes
  the empty database and copies it to `fsyncs.db` beside it. The copy is an
  8 KB file, under the tool, before the program.
- **Nit: make-sqlite's header said one competitor.** Two.
- **Nit: `ftime.ratio` compares a compile under the tool with a bare one.**
  Said beside the hold: part of the ratio of about 8 is the tool's, and 3
  holds with it taken out. The `about` now says eight, not ten.
- **Nit: `watch.py` three times, and a one-cpu machine.** One file,
  `cases/shared/watch.py`. It exits with the reason when the contended cpu
  is the only one it may use, and both contention cases need
  `test "$(nproc)" -ge 2`, so such a machine skips them with that line.
