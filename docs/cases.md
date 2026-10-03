# Cases: programs nobody wrote for Omniscient

ROADMAP phase 12. Every other ground truth in this repository is a workload
written for its test. A **case** is a program that was not -- the
distribution's `sqlite3`, CPython, `gcc`, `make` -- with a cause put in front of
it from outside, and an account of that cause the tool never reads. The
question is the one a person asks: *why is this slow, for this objective?* The
case holds the answer against the known cause.

`tests/cases.rs` runs every case. `cargo test --test cases` after `make` (the
eBPF sources need the capabilities `make` grants; without them each cause skips
with the machine's reason).

## What a case checks

Every cause of every case is asked **both ways**:

- **as a person would**: `omn latency --objective <objective> --format json --
  <program>`, read from its verdict;
- **as an agent would**: the same question's session (`omn latency
  --dump-session`), approved by the person who starts `omn-mcp` (it is written
  into the `--allow-launch` directory), run through `run_session`, and read back
  the way `docs/mcp.md` tells a model to read it -- the biggest row of the
  decomposition, and the dominant stack of the request that row came from.

Each answer is held to named checks:

| Check | What it asserts |
|---|---|
| `the-first-suspect-is-the-known-cause` | the dominant state (and, for the person's way, the verdict's `limit`) is the cause the case put there |
| `the-dominant-stack-ends-in-the-known-frame` | the hottest function on a cpu, or the frame the dominant block began in off it, is the one the case names -- compared only where the frame is named, never a name against an address |
| `the-block-s-frames-are-named` | the dominant stack passes through the frame the case names under `through` |
| `every-cited-number-agrees-with-the-reference` | each row the case lists under `agrees` is within the two resolutions of the reference's figure |
| `the-verdict-says-how-it-knows` | the method, the evidence level of every row, the unwind mode, what it could not see (losses, degradations, missing rows), and for the agent's way where its names came from |
| `the-person-s-way-names-the-hottest-function` | where the case asks for names, the verdict's hottest function is one |
| `the-person-s-way-says-where-names-came-from` | the verdict's notes say where its names came from |
| `the-sentence-claims-nothing-false` | the verdict's sentence contains none of the case's `never_says` |
| `both-ways-name-the-same-suspect` | the two answers name the same state and, where both are named, the same leaf |

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

## The format: `cases/<name>/case.yml`

The directory is the case: `case.yml`, and any helper the injection runs (a
script that holds a lock). The directory's name is the case's `name`.

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
      through: sqliteDefaultBusyCallback
      never_says: [not a lock]
      agrees:
        - row: on-cpu           # a row's amount, both ways
          with: timer.cpu_ns    # <reference>.<key>
          outside: timer.real_ns   # what the account covers of the life; the rest is slack
          relative: 0.01        # a fraction of the reference, with the reason in a comment
          slack_ns: 100000000   # a fixed slack, with the reason in a comment
          plus: holder.poll_ns  # a reference figure added to the slack
open_findings:
  - check: the-sentence-claims-nothing-false
    filed: docs/fixes/w17-cases.md, "a polled lock is called not a lock"
```

**Words.** `{dir}` is the case's scratch directory (under the system's
temporary directory, removed afterwards), `{case}` its directory in the tree,
`{cache}` where fetched files are kept: `$XDG_CACHE_HOME/omniscient/cases/<name>/`
(`~/.cache/...` when it is unset), never the tree.

**`own_account`.** `stdout` means the program writes its account there; the
harness launches it as `sh -c 'exec "$@" 1>&2' sh <target>`, so the account
arrives on its stderr, which both ways read back (`omn`'s stderr, and
`run_session`'s `said_by_omn`). The `exec` keeps the pid: the program's main
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
| cpu contention | a competitor pinned to the target's cpu (the competitor, never the target) | the target's `/proc/<pid>/schedstat` |
| slow storage | `dm-delay` over a loop device named `omn-*` (privileged) | the delay times the I/O count from `/proc/<pid>/io` |

## The cases

| Case | Program | On the cpu | Off it | Reference |
|---|---|---|---|---|
| `sqlite3-stripped` | the distribution's `sqlite3`, stripped (names by build id through debuginfod) | a join whose plan scans `t` 200 times instead of searching its index | a second process holding `BEGIN EXCLUSIVE` 1.5 s from the moment the target appears, the target in its busy handler | `EXPLAIN QUERY PLAN`, `.stats`, `.timer`; the holder's clock |

Measured on the development machine (24 cpus, five other agents running, both
ways each):

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

What the case shows wrong is in `docs/fixes/w17-cases.md`, each with the check
that fails: the person's way cannot ask debuginfod and ranks a stripped
binary's instructions rather than its functions; its verdict drops where its
names came from; an off-cpu stack cannot be named by debuginfod; and the
sentence calls sqlite's polled lock "something that is not a lock".

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
