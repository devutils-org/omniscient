# Cases: programs nobody wrote for Omniscient

ROADMAP phase 12. Every other ground truth in this repository is a workload
written for its test. A **case** is a program that was not -- the
distribution's `sqlite3`, CPython, `gcc`, `make` -- with a cause put in front of
it from outside, and an account of that cause the tool never reads. The
question is the one a person asks: *why is this slow, for this objective?* The
case holds the answer against the known cause.

`tests/cases.rs` runs every case. `cargo test --test cases` after `make` (the
eBPF sources need the capabilities `make` grants; without them each cause skips
with the machine's reason). Two variables narrow or keep a run:
`OMN_CASES_ONLY=make-sqlite,gcc-shell` asks those cases only, and
`OMN_CASES_KEEP=<dir>` writes each answer as it was given,
`<case>-<cause>-<cli|mcp>.json`, for reading an outcome back.

## What a case checks

Every cause of every case is asked **both ways**:

- **as a person would**: `omn latency --objective <objective> --format json --
  <program>`, read from its verdict;
- **as an agent would**: the same question's session (`omn latency
  --dump-session`), approved by the person who starts `omn-mcp` (it is written
  into the `--allow-launch` directory), run through `run_session`, and read back
  the way `omn latency` reads the same run ("The agent's reading", below).

Both ways run the **same binary with the same privileges**: `omn-mcp` collects
through the `omn` beside it, so the capabilities `make` grants are on one file.
The person's way reads them off `target/debug/omn` before it asks; every
answer of `run_session` carries `answered_by` (the binary and its
`file_capabilities`). Two ways that ran with different privileges are not
compared -- what differs between them is the machine's -- and the cause fails
`both-ways-run-with-the-same-privileges` with both. One file read twice can
only differ when the file changed between the two asks, so the check also
reads what the runs themselves were served by: every state both ways read
must come from the same source and be of the same kind (`traced`,
`sampled`). A process that could not use the file's capabilities --
`no_new_privs`, a `nosuid` mount -- falls back to another backend, and its
rows say so there. A fallback of the same source and kind is not seen: the
person's JSON names no backend per row.

Each answer is held to named checks:

| Check | What it asserts |
|---|---|
| `the-first-suspect-is-the-known-cause` | the dominant state (and, for the person's way, the verdict's `limit`) is the cause the case put there |
| `the-dominant-stack-ends-in-the-known-frame` | the hottest function on a cpu, or the frame the dominant block began in off it, is the one the case names -- compared only where the frame is named, never a name against an address. Under `names_no_function`, the answer names no hottest function at all (a flat interpreter profile) |
| `the-block-s-frames-are-named` | the dominant stack passes through the frame the case names under `through`; unchecked where names beyond the file's own (debuginfod) were asked for and could not be had, and a frame is an address; under `symbols: local` an absent frame fails |
| `every-cited-number-agrees-with-the-reference` | each row the case lists under `agrees` is within the two resolutions of the reference's figure |
| `the-verdict-says-how-it-knows` | the method, the evidence level of every row, the unwind mode, and for the agent's way where its names came from. What it could not see (losses, degradations, missing rows) is printed under the run's line, not required: a run that lost nothing is not a worse answer |
| `the-person-s-way-names-the-hottest-function` | where the case asks for names, the verdict's hottest function is one |
| `the-person-s-way-says-where-names-came-from` | the verdict's notes say where its names came from |
| `the-sentence-claims-nothing-false` | the verdict's sentence contains none of the case's `never_says`; for the agent's way, the frames of the stack it names contain none |
| `the-sentence-says-what-the-case-knows` | the verdict's sentence contains each of the case's `says` |
| `both-ways-name-the-same-suspect` | the two answers name the same state, and the same leaf where the case names its known frame (`leaf`); unchecked, with both leaves, when the states agree and a leaf is an address. A cause that names no frame is about a state: the hottest leaf of a spread profile -- a kernel function at 5 % of the samples, the next at 3.5 % -- moves between two runs, and is not compared; whether each names one at all still is, so one way naming a function where the other names none fails |
| `both-ways-run-with-the-same-privileges` | the person's `omn` and the binary `run_session` answered through are one file with the same capabilities, and every state both ways read came from the same source, of the same kind |
| `the-verdict-s-json-spells-its-limit-as-the-text-does` | the person's JSON writes the verdict's `limit` as the text and the catalog spell it (`cpu-queue`, not `cpu_queue`). The other checks read the limit in the text's spelling, so this one alone holds the JSON's |

A check is **ok**, **failed**, or **unchecked** with the reason (names that were
not asked for, a leaf that is an address). A cause whose reference says the
cause was not there to be found -- the lock released before the target asked for
it -- **skips** with the numbers: that run asked a different question. A cause
this machine cannot serve skips with the machine's reason on the skip line, the
layout `.github/scripts/skip-gate.sh` reads.

**A case never carries a workaround.** A check a case shows failing is a finding
against the file that produced it -- the method, the collector, the unwinder --
filed, and listed under the case's `open_findings`. While it fails, the test
prints `FINDING` with where it was filed and passes; once it passes everywhere
it was checked, the test fails until the line is removed and the finding
closed. So a fix elsewhere is noticed here, and a regression of a closed one is
a failure.

A finding whose check **alternates** -- the verdict decided or not by a few
percent, a row within its slack in one run of four -- cannot be held by that
rule: listed, its passing runs fail the test; unlisted, its failing ones do.
It is listed with `sometimes: true` and its filing says why it alternates and
how often. Its failure prints `FINDING`; its pass prints a `note` and asks
nothing, and the line is closed by hand, with the runs that show it closed.
This is the open finding's form of the flaky tally, and as narrow, given a
`cause:`: a finding names the cause it was filed against, and the same check
on the case's other causes is still a failure, as it is on every other case.
A finding with no `cause:` covers every cause of the case, and is filed so
only when it was seen on each of them.

## The format: `cases/<name>/case.yml`

The directory is the case: `case.yml`, and any helper the injection runs (a
script that holds a lock). The directory's name is the case's `name`. A helper
more than one case calls lives in `cases/shared/` (`{case}/../shared/watch.py`).

```yaml
name: sqlite3-stripped          # the directory's name
about: >-                       # what the program is, in a sentence or two
  ...
program:
  command: sqlite3              # on PATH
  obtained: >-                  # how it was obtained: package, build id, flags
    the distribution's package, stripped, ...
  version: [sqlite3, --version] # printed with the case's results
  stripped: true                # checked: a file with no .symtab, or the case skips
needs:                          # each must exit 0, or the case skips with its line
  - run: [python3, -c, 'import sqlite3']
    for: the second process that holds the lock
symbols: debuginfod             # local (the default) or debuginfod
fetch:                          # files not on the machine, fetched once
  - url: https://...
    sha256: ...
    file: sqlite-amalgamation.zip   # kept as {cache}/<file>
setup:                          # commands run once, in {dir}, before any cause
  - [sqlite3, '{dir}/db.sqlite', 'CREATE TABLE ...']
causes:
  - name: a-plan-that-scans
    on: cpu                     # cpu or off-cpu; a case has at least one of each
    question: latency           # the triage command asked; only `latency` yet
    objective: latency
    injection:
      kind: input               # input, process, build or cache
      about: >-                 # what was put in front of the program
        ...
      run: [python3, '{case}/holder.py', ...]   # `process` only: started first
      ready: held               # the line it prints once the cause is in place
    target: [sqlite3, '{dir}/db.sqlite', .timer on, 'SELECT ...']
    own_account: stdout         # where the program writes its account (below)
    reference:                  # each read by a named extractor
      - name: timer
        extractor: sqlite3-timer
        from: target            # the measured run's own account
      - name: holder
        extractor: key-value
        from: injection         # what the injecting process printed
      - name: plan
        extractor: sqlite3-plan
        run: [sqlite3, '{dir}/db.sqlite', 'EXPLAIN QUERY PLAN ...']   # a command of its own
    holds:                      # the cause was there to be found, or the cause skips
      - {value: plan.scans, at_least: 2, why: both loops are scans}
    expect:
      limit: cpu                # the person's verdict's `limit`
      state: on-cpu             # the dominant row, before any `:` qualifier
      leaf: sqlite3VdbeExec     # a prefix: `.lto_priv.0` and `.isra.0` suffixes match
      # names_no_function: true # instead of `leaf`: no hottest function is named
      through: sqliteDefaultBusyCallback
      never_says: [not a lock]
      agrees:
        - row: on-cpu           # a row's amount, both ways
          with: timer.cpu_ns    # <reference>.<key>
          outside: timer.real_ns   # what the account covers of the life; the rest is slack
          relative: 0.01        # a fraction of the reference, with the reason in a comment
          slack_ns: 100000000   # a fixed slack, with the reason in a comment
          plus: holder.poll_ns  # a reference figure added to the slack
      says: [a lock polled with a sleep between tries]   # what it must say
open_findings:
  - check: every-cited-number-agrees-with-the-reference
    filed: docs/fixes/w18-cases.md, "a job is a chain of processes"
  - check: the-first-suspect-is-the-known-cause
    cause: the-critical-path-is-one-job   # this cause only; absent, every cause
    sometimes: true             # fails in some runs only: see the rule above
    filed: docs/fixes/w18-cases.md, "a job is a chain of processes"
held: >-                        # optional: not asked, still checked for form
  why, and where that was filed
```

**`held`.** A case that cannot be asked yet -- a check that gives a different
answer from run to run, which the open-finding rule cannot hold -- stays in
`cases/<name>/` with `held:` saying why and where that was filed. The test
prints `HELD case <name>: <why>` and asks nothing, and
`every_case_is_well_formed` still parses it, so it does not rot while it waits.

**Words.** `{dir}` is the case's scratch directory (under the system's
temporary directory, removed afterwards), `{case}` its directory in the tree,
`{cache}` where fetched files are kept: `$XDG_CACHE_HOME/omniscient/cases/<name>/`
(`~/.cache/...` when it is unset), never the tree.

**`own_account`.** `stdout` means the program writes its account there; the
harness launches it as `sh -c 'exec "$@" 1>&2' sh <target>`, so the account
arrives on its stderr, which both ways read back (`omn`'s stderr; and
`run_session`'s `said_by_omn` followed by the whole of its `program_stderr`,
where `run_session` puts the program's stderr apart from `omn`'s lines). The `exec` keeps the pid: the program's main
thread is still the one the triage decomposes. Nothing about the program
changes.

**Agreement.** A row agrees with its reference when the two are apart by no
more than the row's own resolution, the reference's resolution, and the case's
named slacks. Each slack is written next to its reason: an extractor states the
resolution it was printed at, and the case states what the reference does not
cover.

## The extractors

A small registry in `tests/cases.rs` (`fn extractor`), keyed by name. A new
kind of account is one function there; a case names it.

| Name | Reads | Gives |
|---|---|---|
| `sqlite3-timer` | `.timer on`'s `Run Time: real R user U sys S`, summed over statements | `real_ns`, `cpu_ns` (user plus sys), `off_cpu_ns` (real less cpu); each to the microsecond it is printed at, per statement |
| `sqlite3-stats` | `.stats on`, the last statement's block | `fullscan_steps`, `vm_steps`, `sorts`, `autoindex_inserts`, exact |
| `sqlite3-plan` | `EXPLAIN QUERY PLAN` | `scans`, `searches`: the plan's loops that scan a table and that search a key or an index |
| `key-value` | `key=value` words on any line, numbers only, the last of each | every key, at resolution 0: the case states the slack |

Three accounts are read through `key-value` by a helper of their case that
prints them as such: `gcc -ftime-report` (`cases/gcc-shell/ftime.py`), `strace
-c` (`cases/sqlite3-durability/fsyncs.py`) and the kernel's `schedstat` of a
process tree (`cases/shared/watch.py`). None is registered as an extractor of
its own yet.

## Injection, without touching the target

The tool never injects anything (boundary 5), and neither does a case into the
program: the cause is put in front of it. The ways the phase-12 draft lists,
each with its reference:

| Injection | How | Reference |
|---|---|---|
| an input that defeats an index | the same program and data, a query whose plan scans | `EXPLAIN QUERY PLAN`, `.stats`' fullscan steps |
| a lock held by someone else | a second process holds the resource (`BEGIN EXCLUSIVE`, `flock`) | the holder's own clock around the hold |
| a cold page cache | `posix_fadvise(POSIX_FADV_DONTNEED)` on the input, from the harness | `mincore` before the run |
| memory pressure | `systemd-run --user --scope -p MemoryMax=...` | the cgroup's `memory.stat`, `memory.events` |
| cpu contention | two competitors spinning on cpu 0, started by `cases/shared/watch.py`, and the target launched on that cpu by `taskset -c 0` (an affinity given at exec, the program unchanged) | `watch.py`: the `schedstat` of every task under the target, read from another cpu once a millisecond |
| slow storage | `dm-delay` over a loop device named `omn-*` (privileged) | the delay times the I/O count from `/proc/<pid>/io` |
| a program that is many processes | nothing: the shape of the build is the cause (`make -j8`, a compiler driver) | `watch.py`, as for cpu contention: per job, its life, its subtree's cpu and wait for a cpu |

**`cases/shared/watch.py`** is the reference of every case whose program is a
process tree, or whose cause is a shared cpu: it waits for the target by its
first word and the case's scratch directory, reads every task's `schedstat`
under it from outside, never touching any, and prints one `key=value` line
when the target is gone. It first ends whatever an earlier run of the case
left behind. Its docstring lists every key.

## The cases

| Case | Program | On the cpu | Off it | Reference |
|---|---|---|---|---|
| `sqlite3-stripped` | the distribution's `sqlite3`, stripped (names by build id through debuginfod) | a join whose plan scans `t` 200 times instead of searching its index | a second process holding `BEGIN EXCLUSIVE` 1.5 s from the moment the target appears, the target in its busy handler | `EXPLAIN QUERY PLAN`, `.stats`, `.timer`; the holder's clock |
| `sqlite3-durability` | the distribution's `sqlite3`, stripped, committing single-row INSERTs, each its own commit, on a disk | 50 000 commits in a write-ahead log, `synchronous=NORMAL`: syncs only at checkpoints | 500 commits with a rollback journal, `synchronous=FULL`: four `fdatasync` a commit | `.timer`; the syncs counted by a second, traced run (`strace -c`); the program's `schedstat` (`cases/shared/watch.py`) |
| `make-sqlite` | GNU make 4.4 rebuilding sqlite's autoconf release from its one source (`-W sqlite3.c`, `-O0`) | the critical path is one job of four, the shell's | the library's object built on cpu 0 beside two spinners pinned there | `watch.py`: per job, its life, its subtree's cpu and wait for a cpu |
| `cpython-interpreter` | python3 3.14, no perf map | `score`, made dominant by the input | a read of a named pipe written 1.5 s after the target appears | cProfile, the program's clocks; the writer's clock |
| `cpython-perf-map` | the same under `-X perf`; names from the perf map | as `cpython-interpreter` | as `cpython-interpreter` | as `cpython-interpreter` |
| `http-server-stdlib` | `python3 -m http.server` | a 30 000-entry listing | a slow client; a cold 256 MiB file, skipped where storage is fast | the client's clock, the serving thread's schedstat, the process's stat |
| `gcc-shell` | gcc 16 compiling sqlite's `shell.c` | the passes `-O2` adds over `-O0` | the compile on cpu 0 beside two spinners | `-ftime-report`; `watch.py` |

`gcc-shell` was held back in `cases/held/` during the seventeenth wave, while
the person's way named the driver's `wait4` and the agent's read whichever
table weighed most; both now follow the wait into the compiler, and it is asked
with the others. Each case's measurements are in its handoff:
`docs/fixes/w17-build.md`, `docs/fixes/w17-python.md`,
`docs/fixes/w18-cases.md`.

`sqlite3-stripped`, measured on the development machine (24 cpus, five other
agents running, both ways each):

```
a-plan-that-scans/cli:     on-cpu 1456.4 ms   .timer cpu 1440.3 ms of real 1452.9 ms   40 M fullscan steps, plan: 2 scans, 0 searches
a-plan-that-scans/mcp:     on-cpu 1417.3 ms   .timer cpu 1402.0 ms of real 1413.2 ms   hottest sqlite3VdbeExec (debuginfod)
a-lock-held-elsewhere/cli: blocked 1530.2 ms  holder 1500.1 ms after it saw the target; .timer off-cpu 1530.3 ms
a-lock-held-elsewhere/mcp: blocked 1529.2 ms  holder 1500.1 ms;                        .timer off-cpu 1529.4 ms
```

The on-cpu row exceeds `.timer`'s cpu by about 1 %: a time-based sample counts
the interrupts that ran on the task's cpu, which getrusage leaves out
(`cpu.profile`'s own caveat). The blocked row exceeds the holder's clock by up
to the 100 ms sqlite's busy handler sleeps between its last retries.

What the case showed wrong is in `docs/fixes/w17-cases.md`, each with the
check that failed: the person's way could not ask debuginfod and ranked a
stripped binary's instructions rather than its functions; its verdict dropped
where its names came from; an off-cpu stack could not be named by debuginfod;
the sentence called sqlite's polled lock "something that is not a lock: a
timer, a pipe or a socket"; and `run_session`'s required budget, a share of
the target's own cpu, stopped the lock's run at 100 %. Every one of them is
closed by the eighteenth wave: the case lists no open finding.

## The agent's reading

`docs/mcp.md` tells a model how to read `run_session`'s answer; the agent's
way reads it so, and the way `omn latency` reads the same run
(`src/triage/latency.rs`, `Tasks::follow`; `src/triage/diff.rs`, `hottest`):

- **Which task.** The first table of the decomposition is the task whose rows
  account for the most of the life: the program's main thread. While the
  table read has `blocked` as its largest row, and another task not yet read
  was busy -- on a cpu, or queued for one -- for at least a quarter of that
  block, that task's table is read instead, with the block as its whole: a
  child the program waited for, or a thread that ran while the main thread
  slept. Up to eight steps. The verdict's `task` says which task the person's
  way named, and the run's line prints the agent's path (`followed: task:1
  blocked 5346.0 ms -> task:9`).
- **Its stack, its own.** A `stack` output adds up every task's events, so
  the stack of a followed task cannot be read from it. The approved session
  asks `raw` beside each `stack`, and the agent reads the request's every
  event (`read_run`, `max_events: 0`) for the task it reads: off the cpu, the
  stack with the most blocked nanoseconds (`total_ns` less `queued_ns`); on
  it, each sample charged to the innermost frame a perf map named
  (`/tmp/perf-<pid>.map`, cited by the profile's names note, and last
  written once the run was asked for: an older map is an earlier process's
  with the same pid, not read and said), or to its leaf.
- **A flat interpreter profile names nothing.** When the program is an
  interpreter -- by the name a word of its command goes by, or by the module
  of the unnamed frames of half its samples -- and the hottest function was
  charged by its leaf and holds less than half the samples, no function is
  named.

**Names, the agent's way.** Where the case asks for `symbols: debuginfod`, the
agent's session asks it of every request, and takes it back from each source
`omn` refuses it for (`omn -f <session> --dump-resolved`, exit 3, *"is not
implemented for `<source>`"*). The sources that read `--symbols` are `omn`'s
to say: one that starts to is asked without a line of the harness changing.
The person's way passes `--symbols` as soon as `omn latency --help` lists it,
and asks whatever the triage sets it on.

## Adding a case

1. A directory `cases/<name>/` with `case.yml`, and any helper its injection
   runs. Nothing under `src/`.
2. Two causes at least, one `on: cpu` and one `on: off-cpu`, each with a
   reference the tool does not read, and `holds` that say when the cause was
   there to be found.
3. A reference of a new shape: one extractor in `tests/cases.rs`, registered by
   name, with a unit test on text the program prints.
4. `cargo test --test cases`. A check that fails is a finding: file it against
   the file that got it wrong and list it under `open_findings`; never adjust
   the case until it passes.
5. A program the CI's runner does not carry: the workflow installs it (a
   missing workload is not an expected skip).
