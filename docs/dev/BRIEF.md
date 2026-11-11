# Brief — the settings beside the measurement, and advice that says how to check it

The thirty-first wave. The user asked whether omniscient could also be a
diagnostic tool: use what the kernel exposes (sysctl, sysfs, cgroups,
`/proc/<pid>`, the command line, Kconfig) to advise how to configure a target
process or the machine. This wave answers yes, within the boundary the project
already set:

> **It observes; it does not tune.** (`docs/DECISIONS.md`, boundary 5.)
> Omniscient will report that readahead is not being used; the person decides
> what to do about it.

So the wave builds **advice, never application**. A tool that measures and
changes in one run produces a second reading of a machine the first one
altered, which nobody can interpret. Advice keeps the loop readable: measure,
advise, *the person* changes the setting, measure again, and `omn diff` says
whether the change did what the advice predicted.

Read `CLAUDE.md` first, including "How much to test, and when", then
`docs/dev/STATE.md`, then `docs/triage.md` (the layer this one extends), then
`docs/contributing.md` ("Ground truth is the bar"). The earlier briefs are in
`docs/dev/briefs/`; the twenty-ninth's is `wave-29-func.md`.

**This is the only brief.** What comes after this wave is written in
`docs/dev/STATE.md` under "What remains", and nowhere else.

---

## What already exists, and what is missing

**The symptoms are sources already**: `mem.oom` (kills, and the victims with
their `oom_score_adj`), `mem.cgroup`, `mem.pressure`, `cpu.pressure`,
`io.pressure`, `mem.thp`, `mem.numa`, `mem.reclaim`, `mem.faults`,
`sched.irq`, `sched.numa_balance`, `net.softirq`, `power.freq`,
`io.readahead`, and the rest of the catalog.

**The triage layer exists** (`src/triage/`): `omn latency`, `omn saturate` and
`omn diff` take a question and an objective, choose the measurements, and read
the answer off the engine's results. A triage request without an objective is
refused, never defaulted (`docs/DECISIONS.md`, "Settled in the tenth wave").
That rule matters more for advice than it did for a verdict: THP, the
governor, `vm.swappiness` and the preemption model give *opposite* advice for
throughput, tail latency and energy.

**Some of the settings are already recorded**, as context:
`capabilities::Environment` (`src/capabilities.rs:175`) carries the governor,
the THP mode, SMT, `perf_event_paranoid`, the clocksource and the
mitigations, and every resolved run writes them out. They are a fixed struct,
read once, machine-wide, and nothing reads them back to explain a number.

**What is missing**:

1. The settings as data that can be asked for, named, and compared:
   machine-wide *and* the target's own (its cgroup's limits, its
   `oom_score_adj`, its scheduling policy and affinity, its I/O priority, its
   rlimits, its memory policy, the THP state of its mappings).
2. A rule that goes from a measured symptom to the mechanism behind it, the
   setting that governs that mechanism, its current value, what changing it is
   expected to do *to which reading*, and the run that checks it.
3. Ground truth for each rule: a fixture where the setting is changed and the
   reading moves the way the rule says.

---

## The decisions this wave takes, for `docs/DECISIONS.md`

The coordinator writes these as "Settled in the thirty-first wave", and
amends boundary 5 with one sentence pointing at them. They are listed here so
every stream builds against the same ones.

| Question | Answer | Why |
|---|---|---|
| Does omniscient change a setting? | **Never.** No write to `/proc/sys`, sysfs, a cgroup file or a process attribute, on any path, in any mode. A run still needs no privilege it did not need before. | Boundary 5 stands unchanged. The tests check it (stream A). |
| Does it print the command that would change one? | **Yes, as text**, under the advice, with the scope it acts on (this process, this cgroup, the machine until reboot, the machine persistently). | The person needs to know what to type. Printing a command is not running it. The scope is printed because `sysctl -w` on a shared machine is a decision about other people's processes. |
| Is there advice without a measurement? | **No.** A rule fires only on a reading of *this* run, and the advice quotes that reading. No "good practice" list, no default tuning profile. | The "hundred sysctls to speed up Linux" lists are what this must never become. A setting that is unusual but did not show up in any reading is not advice. |
| Is there advice without an objective? | **No: refused with the list**, like every triage request. | The same setting is right for one objective and wrong for another. |
| What does advice say? | The evidence (source, reading, value), the mechanism (one sentence, and the kernel document), the setting (its path and its current value, or `unknown` with the reason), the expected effect on a named reading, the scope of the change, the verification (`omn diff` on two runs, the exact request) and the rule's caveats. | Each part is what separates advice from an opinion. The verification is the one that keeps it honest. |
| A setting that cannot be read | **`unknown`, with the reason** (no such file on this kernel, a mount not there, permission). Never a guessed default. A rule whose decision depends on that setting says it cannot decide, rather than advising on an assumption. | The first standing invariant: unknown never counts as available. |
| A setting that changed during the run | **Read at the start and at the end; a difference is a finding**, printed above any advice, and the rule that reads it does not fire. | Something else on the machine (tuned, systemd, a person) changed it, and the readings straddle two configurations. |
| Where rules live | **In the catalog, as data** (`catalog/advice.yaml`), not in code, held by the loader. Each rule names its objective, its evidence, its setting, its expected effect, its verification and its ground-truth test. A rule naming a source, setting or reading that does not exist is a load error. | Same as sources: the CLI, `omn list` and the validator are projections of the catalog. |
| How a rule proves itself | **A rule without a passing ground-truth test is not loaded** in a release build. The test applies the setting *in the fixture* (the fixture is the test, not omn), runs omn before and after, and checks the reading moved as the rule predicted. | `docs/contributing.md`: ground truth is the bar. A rule nobody has seen work is a blog post. |
| Machine-wide settings in a test | **Only in a distroproof guest, as root.** On the development machine and in CI a test changes only what is scoped to the fixture: its own cgroup (`systemd-run --user --scope`), its own affinity, `prctl`, `madvise`, `posix_fadvise`, its own network namespace. A rule whose ground truth needs a machine-wide write runs under `distroproof run` and is skipped, loudly, elsewhere. | A test suite that changes the developer's `vm.swappiness` is a defect. |

---

## The streams

Three streams, **in order**: each one builds on the previous one's first
commit. Run in one worktree of your own (`claude/w31`), the way the last
waves were run, unless the coordinator splits them.

| | Stream | Delivers | Owns |
|---|---|---|---|
| **S** | the settings | the settings as sources: what is set, for the machine and for the target, at the start and the end of a run | `catalog/conf.yaml` (new), `src/collect/conf/**` (new), `tests/scenario_conf.rs` (new), `tests/family_conf.rs` (new), `examples/conf-*.yml` (new), `docs/scenarios/conf.md` (new), `docs/how-to-use/conf.md` (new) |
| **A** | the advice | the rule format, its loader, `omn advise`, and the advice section in `omn latency` and `omn saturate` | `catalog/advice.yaml` (new), the advice part of `src/catalog.rs`, `src/triage/advise.rs` (new), the calls in `src/triage/{latency,saturate}.rs` and `src/command/triage.rs`, `src/cli.rs` for the new subcommand, `tests/advice.rs` (new), `man/omn.1` |
| **G** | the ground truth | the first rules, each with the fixture that proves it | the rules in `catalog/advice.yaml` (after A's first commit), `tests/advice_truth.rs` (new), `tests/fixtures/advice/**` (new), `docs/scenarios/advice.md` (new) |

**The coordinator owns** `docs/DECISIONS.md`, `docs/ROADMAP.md`,
`docs/CATALOG.md`, `docs/GUIDE.md`, `docs/dev/STATE.md`, `docs/triage.md` and
the session indexes, as in every wave. A decision worth recording goes in the
handoff under "For `docs/DECISIONS.md`".

---

## Stream S — the settings

**A new family, `conf`**: settings are not a measurement of any one
mechanism, they are the context of all of them, and one collector reads them
all. Mode `snapshot`, cost `free`, backends `fs:proc` and `fs:sys` (and
`fs:cgroup` for the target's limits). No eBPF, no privilege.

**The settings are data too**: `catalog/conf.yaml` lists each setting the
family knows. One entry per setting, with its id, the file it is read from,
its scope (`machine`, `cgroup`, `process`, `thread`), its type and unit, the
mechanism it governs, the kernel document that describes it, and the sources
whose readings it affects. Adding a setting is adding an entry.

Three sources:

1. **`conf.machine`**: the machine-wide settings the catalog lists. Start
   with the ones a reading in this catalog can show the effect of, not the
   whole of `sysctl -a`:
   - memory: `vm.swappiness`, `vm.overcommit_memory`, `vm.overcommit_ratio`,
     `vm.dirty_ratio`, `vm.dirty_background_ratio`, `vm.dirty_expire_centisecs`,
     `vm.min_free_kbytes`, `vm.watermark_scale_factor`, `vm.vfs_cache_pressure`,
     `vm.max_map_count`, `vm.panic_on_oom`, `vm.oom_kill_allocating_task`,
     `vm.zone_reclaim_mode`, `kernel.numa_balancing`, THP `enabled`,
     `defrag` and `khugepaged/defrag`, `lru_gen/enabled`, zswap's
     parameters, KSM's `run`;
   - scheduler: `kernel.sched_autogroup_enabled`, `kernel.sched_rt_runtime_us`,
     the preemption model (`/sys/kernel/debug/sched/preempt` where readable,
     the command line otherwise), `isolcpus`/`nohz_full` from the command line;
   - cpu and power: the governor and the energy-performance preference per
     policy, `scaling_min/max_freq`, the boost switch, SMT control, the idle
     states that are disabled;
   - block: per device, the I/O scheduler, `read_ahead_kb`, `nr_requests`,
     `rotational`, `wbt_lat_usec`;
   - network: `net.core.somaxconn`, `net.core.netdev_max_backlog`,
     `net.ipv4.tcp_congestion_control`, `tcp_rmem`, `tcp_wmem`,
     `tcp_notsent_lowat`, `tcp_slow_start_after_idle`, the qdisc per interface
     (netlink, as the `net` family already does), RPS/XPS masks;
   - interrupts: the affinity of each IRQ (`/proc/irq/*/smp_affinity_list`),
     and whether `irqbalance` is running;
   - the kernel: `/proc/cmdline`, the mitigations, and the Kconfig options the
     catalog's sources depend on (`/proc/config.gz` or `/boot/config-*`;
     `unknown` when neither is there).
2. **`conf.process`**: the target's own settings. `oom_score_adj` and
   `oom_score`; the scheduling policy, priority and nice (`sched_getattr`);
   the cpu affinity of each thread; the I/O priority (`ioprio_get`); the
   rlimits (`/proc/<pid>/limits`); the memory policy (`get_mempolicy` cannot
   be asked of another process: `/proc/<pid>/numa_maps`); the THP state of
   its mappings (`smaps`: `THPeligible`, `AnonHugePages`) and `prctl`'s THP
   flag (`/proc/<pid>/status`); its cgroup and that cgroup's limits:
   `memory.max`, `memory.high`, `memory.low`, `memory.swap.max`,
   `memory.oom.group`, `cpu.max`, `cpu.weight`, `cpuset.cpus.effective`,
   `io.max`, `io.weight`, `pids.max`, walked up the hierarchy (a parent's
   limit binds the child; say which level binds).
3. **`conf.changed`**: read at the start and the end of a run, and emits
   only the differences. Any run can ask for it. The advice layer always
   does.

**What a reading says**: the value, and where it came from (the file, or the
syscall). For a setting that could not be read, `unknown` and why. For a
setting the kernel does not have, `absent` and why: no such file on 6.1 is a
fact about the kernel, not an error.

**Fold `Environment` in, do not duplicate it.** The settings
`capabilities::Environment` already records (governor, THP, SMT, paranoid,
mitigations) become readings of `conf.machine`; `Environment` keeps what the
resolver needs to decide a backend and reads it from the same functions. One
reader per file.

**Ground truth**: each setting read is compared, in `tests/family_conf.rs`,
with what a second, independent path gives (`sysctl -n`, `chrt -p`,
`taskset -pc`, `ionice -p`, `prlimit`, `cat` of the cgroup file); and
`conf.process` is checked on a fixture that set its own `oom_score_adj`,
affinity, nice, `SCHED_BATCH` and a `systemd-run --user --scope -p
MemoryHigh=... -p MemoryMax=...`. `conf.changed` is checked on a fixture
that changes its own `oom_score_adj` and affinity half-way through the run.

**First commit**: the family, `conf.process` with the `oom_score_adj` and the
cgroup memory limits, and the reading type. Stream A builds on it.

---

## Stream A — the advice

**The rule format** (`catalog/advice.yaml`), held by the loader:

```yaml
- id: cgroup.memory.high-before-max
  objective: [latency, tail, throughput]
  scope: cgroup
  evidence:
    - {source: mem.oom, reading: kills, above: 0}
    - {source: conf.process, setting: cgroup.memory.high, is: max}
  setting: cgroup.memory.high
  mechanism: >
    memory.max is a wall: past it the cgroup's own OOM killer runs. memory.high
    is a brake: past it the cgroup's tasks are throttled and reclaimed, and
    nothing is killed.
  docs: "admin-guide/cgroup-v2.rst, memory.high"
  expect: {source: mem.oom, reading: kills, goes: down}
  also:   {source: mem.pressure, reading: some_avg10, goes: up}
  change: "systemd-run --scope -p MemoryHigh=<about 90% of MemoryMax> ...  # this cgroup"
  caveats:
    - "Throttling is slower, not free: the kills become stalls, which mem.pressure counts."
  truth: advice_truth::cgroup_memory_high_turns_kills_into_stalls
```

The loader refuses: an unknown source, reading or setting; an `objective`
outside `Objective`; a rule without `expect`, `change` or `truth`; two rules
with one id. A test in `tests/advice.rs` checks that every `truth` names a
test that exists.

**The evidence language stays small**: a reading compared with a constant
(`above`, `below`, `is`), a reading compared with a setting
(`reading: rss, near: cgroup.memory.max, within: 10%`), and `all` of a list.
No expressions, no scripting. A rule that needs more is a sign it is two
rules, or that a source is missing; file it, do not grow the language.

**`omn advise --objective <o> [--pid N | -- command]`**: runs the sessions
the rules for that objective need (the union of their evidence sources, and
`conf.*` always), then evaluates every rule. It prints, in this order:

1. settings that changed during the run, if any (and no advice depends on
   them);
2. the rules that fired, each as the decisions table above says: evidence,
   mechanism, setting and current value, expected effect, scope, the command
   as text, the verification as an exact `omn diff` request, caveats;
3. the rules that **could not decide**, and the setting or reading they lacked
   (`unknown`, a source not available here, losses that make a count a lower
   bound);
4. one line with the number of rules evaluated that did not fire. They are
   not listed unless `-v`.

**In `omn latency` and `omn saturate`**: after the verdict, one section,
`settings that bear on this`. It lists the rules whose evidence is the
verdict's own culprit. It runs no new session: it evaluates the rules on the
readings the triage already has, plus `conf.*`, which is free.

**What is never printed**: a value as "optimal", a setting with no reading
behind it, a rule that fired on readings with losses without saying so.

**The no-write test**: `tests/advice.rs` runs `omn advise` under a seccomp
filter or an `strace -f -e trace=openat,write` and fails if any file under
`/proc/sys`, `/sys` or `/sys/fs/cgroup` is opened for writing, or if
`sched_setattr`, `sched_setaffinity`, `ioprio_set`, `setpriority`,
`prlimit64` with a new limit, `set_mempolicy` or `madvise` is called on
another process. This is boundary 5 as a test.

---

## Stream G — the first rules, each with its ground truth

Four rules. Each is chosen because the fixture can apply its setting on
itself, so the ground truth runs on the development machine and in CI. Each
test runs omn on the fixture with the setting off, then on, and checks
`expect` and `also` against the readings.

1. **`cgroup.memory.high-before-max`** (above). Fixture: an allocator that
   grows past a limit, in `systemd-run --user --scope -p MemoryMax=256M`,
   then the same with `-p MemoryHigh=224M`. Expected: `mem.oom` kills from 1
   to 0, `mem.pressure` up. The case that started the wave.
2. **`oom.victim-choice`**: two processes in one cgroup at its limit; the one
   the operator would rather keep has the higher `oom_score`. Evidence:
   `mem.oom` in trace mode names the victim, `conf.process` gives each
   one's `oom_score` and `oom_score_adj`. Setting: `oom_score_adj` of the
   other one. Expected: the victim changes. Objective: any. (This rule's
   evidence is "the victim was the larger one", and its advice is about
   *which* process dies, not whether: say so.)
3. **`sched.shared-cpu`**: two cpu-bound threads whose affinity puts them on
   one cpu while other cpus are idle. Evidence: `sched.runqlat` on the
   target, `conf.process` affinity, `cpu.oncpu_time` of the others. Setting:
   the affinity. Expected: run-queue latency down, wall time down.
   Objective: latency, throughput.
4. **`mem.thp-compaction-stall`**: an allocator that faults many anonymous
   huge pages on fragmented memory, with `madvise(MADV_HUGEPAGE)`, against
   the same without it (or with `prctl(PR_SET_THP_DISABLE)`). Evidence:
   `mem.thp` and `mem.compaction` stalls on the target, `conf.machine` THP
   `defrag`. Setting: `defrag` (machine) or `MADV_HUGEPAGE`/`PR_SET_THP_DISABLE`
   (the process, printed first because its scope is smaller). Expected:
   compaction stalls down, tail fault latency down. Objective: tail.
   Fragmenting memory reliably is the hard part: if the fixture cannot do it
   in CI, the test runs under `distroproof run` only, and says so.

A fifth, if time allows: **`io.readahead-unused`**, a sequential reader that
calls `posix_fadvise(POSIX_FADV_RANDOM)`, against one that does not.
Evidence: `io.readahead` and the major faults. This is boundary 5's own
example.

For each rule, `docs/scenarios/advice.md` records the numbers measured, the
settings in place, the kernel, and what the rule does not cover.

**Rules for a later wave**, each needing a machine-wide write and so a
distroproof guest: the governor and EPP for a latency objective; `vm.swappiness`
under a swap-heavy load; IRQ affinity against a network server's cpu;
`tcp_notsent_lowat` and the qdisc for a sender; `read_ahead_kb` per device.
Listed in the handoff for `docs/dev/STATE.md`, not built.

---

## What the wave does not do

- **No profile, no "apply all".** No `omn tune`, no generated
  `/etc/sysctl.d` file. The command for each setting is text, one at a time,
  with its scope.
- **No advice from a reading of another run.** The evidence is this run's.
  `omn diff` compares two runs to *verify*, never to advise.
- **No rule for a setting no reading shows the effect of.** `kernel.kptr_restrict`
  and other security settings are read by `conf.machine` where a source needs
  them (they decide backends), and advise nothing.
- **No coverage of the whole kernel.** The set grows one rule at a time, each
  with its ground truth. A rule without one is not shipped.

---

## The handoff

For each stream: the commits, the tests added and what they measured, the
rows for the session indexes, the decisions for `docs/DECISIONS.md`, and the
cells the portability matrix should move (`conf.*` is `free` everywhere, so
every cell's usable count goes up by three, with its command). The coordinator
then runs the full suite once, on a quiet machine, and writes "After the
thirty-first wave" in `docs/dev/STATE.md`.
