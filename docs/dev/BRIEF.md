# Brief — the settings beside the measurement, and advice that says how to check it

The thirty-first wave. The user asked whether omniscient could also be a
diagnostic tool: use what the kernel exposes (sysctl, sysfs, cgroups,
`/proc/<pid>`, the command line, Kconfig) to advise how to configure a target
process or the machine. The answer is yes, within the boundary the project
already set -- *it observes; it does not tune* (`docs/DECISIONS.md`,
boundary 5): **advice, never application**. A tool that measures and changes
in one run produces a second reading of a machine the first one altered; so
the person changes the setting, and a second run `--against` the first says
whether the reading the advice named moved the way it said.

Run on 2026-11-11, after the user's *"go implementer tout ça"*, in one
worktree (`omniscient-w31`, branch `claude/w31`), the three planned streams
one after another: each built on the previous one's first commit.

## What it built

1. **The `conf` family** (`catalog/conf.yaml`, `src/collect/conf/`):
   `conf.machine`, `conf.process` and `conf.changed`, `fs:proc`, free, no
   privilege. The settings are a table, `catalog/settings.yaml`: about
   seventy rows, each with its reader (a file, with one `*` for instances; a
   `/proc/<pid>` file, a `Name:` line or a `stat` field; a cgroup file read at
   each level; a command-line parameter; a Kconfig option; `ioprio_get`), its
   scope, the mechanism it governs, the kernel document, and the sources
   whose readings it bears on. A setting is `set`, `unset`, `absent` or
   `unknown` with a reason, never a default; a cgroup limit names the level
   that binds; `oom_score` is `computed`. `conf.changed` reads them at every
   poll and emits what differs between the first read and the last. A new
   unit, `setting`.
2. **The advice rules** (`catalog/advice.yaml`, `src/triage/advise.rs`): each
   names its objectives, its evidence (measures compared with constants,
   settings compared with values or with another setting), its setting, the
   reading changing it should move, side effects, the command and its scope,
   caveats, and the test that proves it. The loaders hold both tables to the
   catalog.
3. **`omn advise --objective O [--save F] [--against F] -- PROGRAM`**: one
   session of every measure the objective's rules name plus the settings;
   each rule held, held back or undecided with what it lacked; and, against
   an earlier answer, each prediction checked and the settings that differ.
   A move whose machine or cgroup setting did not change is
   `setting_unchanged`, not the rule's.
4. **`omn latency` and `omn saturate`** read the two settings snapshots too
   and print `settings that bear on this`: the rules their own measures hold.
5. **The ground truth** (`tests/advice_truth.rs`, `tests/fixtures/advice/`,
   `tests/family_conf.rs`), and the boundary as a test:
   `advice_changes_nothing` runs `omn advise` under `strace` and fails on any
   write to a setting or any call that changes another process.

## Measured

`docs/scenarios/advice.md` and `docs/scenarios/conf.md` have every number.
In short:

- `cgroup.memory.swap-denied`: 300 MiB in `MemoryMax=128M` killed in 0.1 s
  with swap denied, completed in 0.7 s with it allowed.
- `oom.victim-by-size`: the first victim moved from the 120 MiB `keeper` to
  the `grower` when the grower's `oom_score_adj` was 1000.
- `sched.shared-cpu`: four threads on one cpu waited 6.8 ms on average in the
  run queue; on four cpus, 5.6 us, and the work took 193 ms instead of 768.
- `io.readahead-refused`: under `POSIX_FADV_RANDOM` nothing was read ahead
  (-3 pages); without it, 1533.

## What differed from the plan

- **The first rule planned was refuted, and not shipped.** `memory.high`
  before `memory.max`: on this machine, with zram, `memory.max` never killed;
  with swap denied, `MemoryHigh=100M` delayed the kill from 0.1 s to 260 s.
  The swap the cgroup was denied is what the measurement supports.
- **THP compaction** was replaced by readahead: a user process here cannot
  fragment memory reliably.
- **The settings table is `catalog/settings.yaml`**, beside `conf.yaml`
  rather than inside it: a table, like `unwind.yaml`, not entries.
- **"Not loaded without a passing test"** became: the table carries only rules
  whose test exists (held by a test) and passes in the suite; a binary cannot
  know whether a test passed.
- **The OOM victims were the machine's** at first: the tracepoint on a
  launched program compared its pid alone. After the wave, the victim program
  follows the target's descendants, and the rule reads the workload's.

## What it leaves

`docs/dev/STATE.md`, "After the thirty-first wave".
