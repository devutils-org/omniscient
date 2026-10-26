# Brief — the escalation, and the debts the tenth wave left

Four work streams. The eleventh fan-out: one stream on the milestone that is
next -- `docs/ROADMAP.md` phase 10's escalation, which the tenth wave's hot attach
made possible -- and three that close what the tenth wave found and filed,
because every one of them is a place where the tool says something it did not
measure, and the escalation would inherit each.

Read `CLAUDE.md` first -- **including "How much to test, and when", which this
wave is held to** -- then `docs/dev/STATE.md`, then your section here, then
`docs/contributing.md`. The ten earlier briefs are in `docs/dev/briefs/`.

**This is the only brief.** What comes after this wave is written in
`docs/dev/STATE.md` under "What remains", and nowhere else.

---

## Before anything else: how you test

The full suite is about 1750 tests and takes 5 to 10 minutes, because some 340
scenarios measure the real machine. Two waves in a row have merged green on
the first full run by holding to this rule:

- **While you work**: run only your own module and your own test file --
  `cargo test --lib <module>` and `cargo test --test <your file>`. Never
  `make check` while iterating.
- **Before each commit**: one `make check`. If it is red, run
  `cargo test --workspace --no-fail-fast` once to see every failure together.
- **A red test outside your paths**: look it up in the flaky tally
  (`docs/open-findings.md` findings 41-46 and the tenth wave's two notes,
  `docs/scenarios/io.md`). If it is there, rerun it alone once and move on. Do
  not rerun the suite for it, and do not try to fix it -- it is not yours.
- **`make`** once per build, before eBPF tests, because a relink drops the
  capabilities. And **run `cargo fmt` before `make`, never after it**: a
  format that changes a source makes the next test build relink `omn`
  without its capabilities, and every eBPF test then fails as unprivileged.
  The coordinator lost a full run to exactly that in the tenth wave.

---

## What changed before this wave

**The tenth wave landed hot attach, the network rows, the bench harness and
triage**, and the merge was green on its first full run. What it left is in
`docs/open-findings.md` under "the tenth wave", one heading per stream, each
with the signature wanted. This wave is those requests, grouped by the files
they land in, plus the escalation.

**The bench has not measured yet.** Its variance gate refuses on a machine
other work is loading -- which is this wave. Its one command (`docs/bench.md`)
is run on a quiet machine, before this wave or after it, never during.

**The kernel's skipped runs are counted on every raw-tracepoint program now**
(the coordinator, on `main`), and a unit test refuses a program that does not.
The first run after that counted skips that had been silent for three waves --
which is the pattern the three debt streams below are about.

---

## The streams

| | Stream | Delivers | Owns |
|---|---|---|---|
| **E** | the escalation | triage names a suspect mid-run, attaches that layer's detailed collectors, detaches the rest, and holds the budget | `src/triage/**`, `src/cli.rs`, `src/request.rs`, `src/session.rs`, `src/collect/mod.rs`, `src/collect/launch.rs`, `src/collect/task.rs`, `tests/triage.rs`, `tests/runner.rs`, `tests/budget.rs`, `examples/triage-*.yml`, `examples/attach-*.yml`, `man/omn.1`, `docs/triage.md` |
| **J** | the engine's debts | the join and the emit layer say what hot attach, the network rows and triage found they cannot | `src/join.rs`, `src/emit.rs`, `src/encode.rs`, `src/render.rs`, `src/main.rs`, `tests/join_engine.rs`, `tests/aggregation.rs`, `tests/encoders.rs`, `tests/vertical.rs`, `examples/join-*.yml`, `examples/vertical-one-request.yml`, `docs/families/join.md` |
| **S** | the silent degradations | the four defects the bench found, and the per-task perf events that do not follow threads | `src/filter.rs`, `src/resolve.rs`, `src/perf.rs`, `src/collect/cpu/**`, `src/collect/proc/**`, `src/collect/mem/page_faults.rs`, `catalog/cpu.yaml`, `catalog/proc.yaml`, `catalog/mem.yaml`, `tests/scenario_cpu.rs`, `tests/scenario_proc.rs`, `tests/scenario_mem.rs`, `tests/family_cpu.rs`, `tests/family_proc.rs`, `examples/cpu-*.yml`, `examples/proc-*.yml`, `docs/fixes/silent.md` (new) |
| **P** | the portability matrix (added mid-wave, when J merged) | the tool run on the kernels and configurations it claims to handle, and a CI that runs what a machine without privileges can | `scripts/matrix.sh` (new), `scripts/vm/**` (new), `tests/portability.rs` (new), `.github/**` (new), `docs/portability.md` (new) |
| **K** | drains that lose nothing | the drained programs stop racing their own drain, and the one test that keeps failing is understood | `src/bpf/map.rs`, `src/bpf/runqlat.rs`, `src/bpf/offcpu.rs`, `src/bpf/proc_syscall.rs`, `src/bpf/sched_*.rs`, `src/bpf/migrate.rs`, `src/bpf/mem_alloc.rs`, `src/bpf/mem_stall.rs`, `src/collect/sched/**`, `tests/scenario_sched.rs`, `tests/family_sched.rs`, `docs/families/drains.md` (new) |

**Two splits to respect.** `src/collect/proc/syscall.rs` is S's and
`src/bpf/proc_syscall.rs` is K's: K changes how the program's rows are drained
and keeps `SyscallProgram`'s public methods as they are, so S's filter fix and
K's drain fix never meet. And `tests/runner.rs` is E's, but two of its tests
are ignored until J and S land their halves -- J's
`a_join_across_a_detached_source_does_not_count_what_fell_after_it_as_a_loss`
and S's `a_counter_attached_late_counts_the_threads_already_running_or_says_it_cannot`.
J and S do not edit that file: each says in its handoff that its half landed,
and the coordinator removes the `#[ignore]` at the merge.

**Nobody owns** `src/catalog.rs`, `src/validate.rs`, `src/capabilities.rs`,
`src/bpf/btf.rs`, `insn.rs`, `attach.rs`, `usdt.rs`, `fentry.rs`,
`src/bpf/mod.rs`, `src/bpf/request*.rs`, `src/collect/{io,net,gpu,power,runtime,mem}/**`
except `mem/page_faults.rs`, or the catalogs not listed. A stream that needs
one writes the request under `## Needs from another stream` in
`docs/open-findings.md`, **under a heading naming the stream**, with the exact
signature, and carries on. That file is append-only per stream.

**The coordinator owns** `examples/README.md`, `docs/GUIDE.md`, `docs/dev/STATE.md`,
`README.md`, `docs/DECISIONS.md`, `docs/ROADMAP.md`, `docs/CATALOG.md` and the `Makefile`.
The session-index test will be red on your branch for the sessions you add.
**That is expected** -- list the rows in your handoff.

---

## Stream E -- the escalation

> **Mission.** `docs/ROADMAP.md` phase 10, item 3: *once the suspect is identified,
> enable that layer's detailed collectors and turn off the rest. The budget
> holds because we only pay dearly for one layer at a time.*

The two halves it stands on exist. Triage names a suspect for an objective
(`src/triage/`), and a request can be attached and detached inside a run
(`window`, the tenth wave's hot attach). What does not exist is the part in
between: **a window decided by what the run has seen so far**, rather than one
written in the session before it started.

**The mechanism is yours to design, and it is the decision worth writing
down.** The runner (`run_all_within` in `src/collect/mod.rs`) attaches a
request at a moment the session gave it. The escalation needs the moment and
the request to come from a decision taken during the run, from the cheap
pass's own numbers -- so something in the runner has to be asked, between two
rounds, whether to attach or detach anything. Keep that question out of the
collector layer's knowledge of triage: the runner asks a trait, the way it
already asks `Cheaper` when a budget is exceeded, and `src/triage/` answers.

**What the escalation has to say, and each is a scenario:**

- **What it escalated to and why**: the cheap pass's verdict, the objective it
  was for, the moment it decided, and the detailed sources it attached. A run
  that escalated and a run that did not are different documents.
- **The budget holds, measured**: the run's own cost (`cost.json`) is under
  the `--budget` it was given, and what it detached to stay there is named.
- **A suspect whose detailed layer this machine cannot serve** is reported as
  such, with the probes' reasons, and the cheap answer stands -- never an
  escalation to a backend that answers a different question.
- **The joins across the switch are honest.** A source attached at the
  escalation has a window, and J is making the join engine respect windows this
  wave; until it lands, say in your handoff which joins you avoided.

**Ground truth**: the triage fixture's workloads, whose limit the test chose.
A program that is cpu-bound for a second and then lock-bound for a second must
escalate to the lock layer after the switch, and its lock waits must match
what the program timed itself -- with the cpu layer's detailed sources
detached.

Also yours, filed with `src/cli.rs` last wave: **`--window` on the command
line**, so the two surfaces build the same `Request` (stream H's request,
under "the tenth wave, stream H"). The flight recorder is **not** this wave: it
is the runner's next piece and follows this one.

---

## Stream J -- the engine's debts

> **Mission.** Four requests the tenth wave filed against `src/join.rs` and
> `src/emit.rs`, each a place where the engine over-states or under-states
> what it knows.

1. **A join across a detached source** (stream H): today the last state open
   before a detach absorbs every event after it -- 63 blocks credited to a
   wakeup that happened before the probes came off. Wanted: `apply_within`,
   `Correlation::outside_window`, states closed when the `from` window closes
   and counted apart from `expired`, and the call site in `src/main.rs` passing
   the windows. The signatures are filed.
2. **The window on `Results`** (stream H): a document says *where* in the run a
   request watched, not only for how long.
3. **A row read through another key is rated by it** (stream N): `net_rx` and
   `net_tx` print `IDENTITY` and are `KEY` -- over-rated, the one direction the
   evidence ladder must never err in. `Part::via`, the weaker of the two keys.
   The two lines of `examples/vertical-one-request.yml` and the assertion in
   `tests/vertical.rs` that flip with it are yours this wave.
4. **A filtered part that matched nothing is a zero** (stream T): the source
   ran, was clean and had events for the key, and none passed the filter --
   today `not read`, which makes every residue `not read`. The unfiltered
   drained case is a decision; write down which way you took it.

**Ground truth**: for 1, the ignored scenario in `tests/runner.rs` says what
the numbers must be (40 sleeps before the detach, 25 after: 40 matched, 25
outside the window, not 67 matched). Say in your handoff that it is ready to
un-ignore; the coordinator does it at the merge. For 3, the vertical case's
rows at `KEY`, with the rows still summing to the client's latency.

---

## Stream S -- the silent degradations

> **Mission.** Five places where the tool accepts a request and then answers a
> different question without saying so -- the class of defect the invariants
> rule out, found by the bench's ground truth and by hot attach.

From `docs/open-findings.md`, "the tenth wave, stream X" and "stream H":

1. **`proc.syscall --filter 'syscall == 110'` drops every call it pushed
   down**: the kernel keeps getppid, then a userspace pass compares 110 with
   the name.
2. **`proc.exec` accepts a filter on `filename` or `comm` and applies
   neither.** Apply it or refuse it; never accept it silently.
3. **`--backend force:` walks past a backend's missing probes** and fails at
   attach after the target was launched -- finding 15's shape through the pin.
4. **`mem.faults` declares `count` and refuses it after the launch.** A
   `PERF_COUNT_SW_PAGE_FAULTS` counting event is the cheapest thing it could
   do; the other answer is the catalog not declaring it.
5. **A per-task counter attached late counts one thread**: 106,206
   instructions for a phase that retired two billion, with no loss and no note.
   The runner already acts on `Prepared::follows_only_later_threads`; the
   better fix is one counter per entry of `/proc/<pid>/task` at the attach.
   Check `cpu.profile`, `sched.switches` and `mem.faults` too, and answer stream
   T's request for a profile that follows threads (`inherit` on a sampling
   event into a per-task ring).

**Ground truth**: each defect's filed command, turned into a scenario that
fails against `main` and passes against your branch; for 5, the ignored test
in `tests/runner.rs`, which your handoff says is ready. Every fix either makes
the answer right or makes the refusal say why -- a sixth silent path is not a
fix.

---

## Stream P -- the portability matrix

> **Mission.** `docs/ROADMAP.md`'s cross-cutting test strategy has two rows nobody
> has built: *Portability -- a VM kernel matrix: floor, LTS, recent, no BTF*,
> and *Degradation -- run without privileges, without BTF, without tracefs:
> check that it degrades while saying so*. Every claim this tool makes about
> another machine has so far been checked on one.

Added when stream J merged, so it starts from a later `main` than the others;
its files are new and nobody else's.

**The matrix.** Boot the tool, built on the host, under other kernels in KVM
(`/dev/kvm` is usable here; `virtme-ng`, installable with `pipx`, boots a
kernel against the host's root filesystem). Kernels: the floor `docs/DECISIONS.md`
settled (6.1), an LTS between (6.6 or 6.12), and the host's own (7.2). Take
prebuilt kernels -- a distribution's, or Ubuntu's mainline builds -- rather
than building one: **this machine has about 9 GB of disk free**, and the
cache lives under `target/matrix/`, which is not committed. A kernel without
BTF does not have to be found or built: hide `/sys/kernel/btf/vmlinux` inside
the guest, and do the same for tracefs, and run once without the file
capabilities, once as root.

**What each cell checks is the capability sweep, across kernels**:

- every source's answer in `omn list --format json` is available, missing or
  unknown with its reason -- and **available means it attaches**: run each
  source reported available for a fraction of a second and fail the cell if
  it refuses at attach (finding 15's shape, on a kernel nobody tried);
- what changes between kernels changes for the reason the catalog gives:
  uprobe-multi is 6.6, so the USDT sources on 6.1 take the uprobe PMU and say
  so; a raw tracepoint a kernel lacks is missing by name;
- a kernel under the floor, or a configuration that removes a requirement, is
  refused with the reason, never degraded silently.

Write each cell's documents under `target/matrix/<kernel>-<config>/`, and
make `tests/portability.rs` assert the invariants over whatever cells exist --
`#[ignore]` for the ones that boot a VM, run through `scripts/matrix.sh`, the
way the bench works; the checks on a cell's documents are cheap and run.

**The CI.** A GitHub Actions workflow under `.github/` that runs `cargo fmt
--check`, clippy and the suite on a hosted runner: no file capabilities, a
distribution kernel, no GPU -- itself one cell of the matrix, and the one
that runs on every push. The eBPF tests skip loudly there, as they are written
to; the workflow must fail on a skip that is not one of those. **Do not push**:
the coordinator decides when the workflow goes to the remote.

`Makefile` is the coordinator's: if a `make matrix` target is wanted, it goes
in your handoff with its exact recipe.

---

## Stream K -- drains that lose nothing

> **Mission.** The eighth wave measured it and nothing has closed it: a drained
> program's userspace drain takes a row with lookup-and-delete while the
> program is a few instructions from adding to it, and the addition lands in a
> freed element. And one scenario keeps failing.

1. **The drain race.** `fentry::HistogramProgram` already solves it: two row
   maps and a generation word, and the drain takes only the map out of use for
   a whole interval (`src/bpf/fentry.rs`, `drain`). `runqlat` and
   `proc_syscall` are the two the eighth wave named; every drained program in
   your files has the same shape -- audit them, and put the two-map scheme in
   `src/bpf/map.rs` once rather than five times. Keep each program's public
   methods unchanged: S owns the collectors of `proc`.
2. **`a_blocking_stack_names_the_call_that_blocked`** failed in two of three
   full runs at the tenth wave's close and passes alone. A test that fails
   repeatedly is the one worth investigating: find whether it is the machine
   or the symbolizer racing the program's exit, and either fix the tool or
   make the scenario assert what the tool can actually promise.

**Ground truth** for the race: a workload that makes a known number of events
drained on a short tick while it runs -- the eighth wave's measurement was
100 000 reads -- and every one of them in a row or in a counted loss, over
several runs. Give every row program you touch a stack-region test and a
byte-coverage test if it lacks one: finding 46 is the file that had neither.

---

## Rules

The three that have held for ten waves:

1. **The unit of ownership is a file set nobody else writes.** The table above.
2. **The handoff is a file only that stream can write**: E's is
   `docs/triage.md`, J's `docs/families/join.md`, S's `docs/fixes/silent.md`,
   K's `docs/families/drains.md`, P's `docs/portability.md`. End it with **the index rows for every
   session you added** and **the ignored tests your half makes ready**.
3. **A task is landed when its scenarios are**: ground truth, the capability
   sweep, the loss path, the refusal.

**Build with `make`, not `cargo build`** -- the preload shim and the
capabilities both fail silently otherwise. One branch per stream, made by
`scripts/spawn.sh`. Commit there; the coordinator merges.
