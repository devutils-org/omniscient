# Brief — the flight recorder, the probes that tell the truth, and a model that reads

Four work streams. The twelfth fan-out: the runner's next piece (`STATE.md`'s
"What remains", item 1), phase 11 now that phase 10's escalation has landed
(item 2), and two streams that close what the eleventh wave measured and filed
-- the portability matrix's ten classes of *available but it does not attach*,
and the kernel-side attributions and drains nobody owned last wave.

Read `CLAUDE.md` first -- **including "How much to test, and when", which this
wave is held to** -- then `STATE.md`, then your section here, then
`docs/contributing.md`. The eleven earlier briefs are in `docs/briefs/`.

**This is the only brief.** What comes after this wave is written in
`STATE.md` under "What remains", and nowhere else.

---

## Before anything else: how you test

The full suite is about 1780 tests and takes 5 to 10 minutes, because some 350
scenarios measure the real machine. The rule:

- **While you work**: run only your own module and your own test files --
  `cargo test --lib <module>` and `cargo test --test <your file>`. Never
  `make check` while iterating.
- **Before each commit**: one `make check`. If it is red, run
  `cargo test --workspace --no-fail-fast` once to see every failure together.
- **A red test outside your paths**: look it up in the flaky tally
  (`docs/open-findings.md` findings 41-46, the "for the flaky tally" notes of
  the tenth and eleventh waves, `docs/scenarios/io.md`). If it is there, rerun
  it alone once and move on. Do not rerun the suite for it, and do not try to
  fix it -- it is not yours.
- **`make`** once per build, before eBPF tests, because a relink drops the
  capabilities. And **run `cargo fmt` before `make`, never after it**: a
  format that changes a source makes the next test build relink `omn`
  without its capabilities, and every eBPF test then fails as unprivileged.

Four streams and a coordinator share this machine. Assert the separation, not
a threshold calibrated on a quiet machine.

---

## What changed before this wave

**The eleventh wave landed the escalation, the engine's debts, the silent
degradations, the drains and the portability matrix**, all five merged on
`main`. `docs/briefs/wave-11-escalation.md` keeps its brief; what it filed is
in `docs/open-findings.md` under "the eleventh wave", one heading per stream,
and finding 48. This wave is those requests, grouped by the files they land
in, plus the two milestones.

**The bench does not run during a wave.** Its variance gate refuses a loaded
machine; `docs/bench.md` has its one command, for after this wave.

**The CI workflow (`.github/workflows/ci.yml`) has never run.** It is not
pushed; the coordinator decides when. Stream V owns it and may change it.

---

## The streams

| | Stream | Delivers | Owns |
|---|---|---|---|
| **R** | the flight recorder | a session whose duration is *until a predicate fires*, with a retention bound and one document per trigger | `src/collect/mod.rs`, `src/collect/launch.rs`, `src/collect/task.rs`, `src/session.rs`, `src/request.rs`, `src/cli.rs`, `src/main.rs`, `src/emit.rs`, `src/encode.rs`, `src/render.rs`, `src/triage/**`, `tests/recorder.rs` (new), `tests/runner.rs`, `tests/budget.rs`, `tests/triage.rs`, `examples/record-*.yml` (new), `man/omn.1`, `docs/recorder.md` (new) |
| **V** | the probes tell the truth | *available means it attaches*, on every kernel the matrix booted: P1-P10, where a predicate runs, and a suite that passes without capabilities | `src/capabilities.rs`, `src/catalog.rs`, `src/validate.rs`, `src/resolve.rs`, `src/filter.rs`, `src/perf.rs`, `src/bpf/mod.rs`, `src/bpf/request*.rs`, `src/bpf/btf.rs`, `src/bpf/attach.rs`, `src/bpf/usdt.rs`, `catalog/*.yaml`, `src/collect/cpu/**`, `src/collect/proc/**`, `src/collect/gpu/**`, `src/collect/power/**`, `src/collect/runtime/**`, `src/collect/mem/page_faults.rs`, `tests/portability.rs`, `tests/scenario_cpu.rs`, `tests/scenario_proc.rs`, `tests/scenario_gpu.rs`, `tests/scenario_power.rs`, `tests/scenario_runtime.rs`, `tests/family_cpu.rs`, `tests/family_proc.rs`, `tests/vertical.rs`, `scripts/matrix.sh`, `scripts/vm/**`, `.github/**`, `docs/portability.md` |
| **D** | the kernel's attributions and drains | a wakeup from an interrupt is not the interrupted task's, and the drained programs outside last wave's files stop racing their drain | `src/bpf/wakeup.rs`, `src/bpf/map.rs`, `src/bpf/fentry.rs`, `src/bpf/io_*.rs`, `src/bpf/net_*.rs`, `src/bpf/sched_*.rs`, `src/bpf/mem_*.rs`, `src/bpf/runqlat.rs`, `src/bpf/offcpu.rs`, `src/bpf/migrate.rs`, `src/bpf/proc_syscall.rs`, `src/collect/sched/**`, `src/collect/io/**`, `src/collect/net/**`, `src/collect/mem/**` except `page_faults.rs`, `tests/scenario_sched.rs`, `tests/family_sched.rs`, `tests/scenario_io.rs`, `tests/scenario_net.rs`, `tests/scenario_mem.rs`, `docs/families/kernel.md` (new) |
| **M** | a model reads it | `ROADMAP.md` phase 11: an MCP server over the resolved file, the results and the catalog -- a second surface on objects that exist | `mcp/**` (new workspace member), the `members` line of `Cargo.toml`, `Cargo.lock`, `docs/mcp.md` (new) |

**Nobody owns** `src/bpf/insn.rs`, `src/join.rs`, `src/stack.rs`,
`src/event.rs`, `src/clock.rs`, the rest of `src/bpf/`, and the tests not
listed. A stream that needs one writes the request under `## Needs from
another stream` in `docs/open-findings.md`, **under a heading naming the
stream and the twelfth wave**, with the exact signature, and carries on. That
file is append-only per stream.

**Three splits to respect.**

- `src/perf.rs` is V's; D's `sched.switches` moves onto its existing
  `ThreadSampler` without changing it. If D needs a change there, it files it.
- `src/bpf/map.rs` is D's; the two-phase drain stream K proposed
  (`Rows::retire`, `take`, `grace_period` made public) is D's half, and the
  collector trait's `retire` and the round loop are R's. Neither is required
  this wave; if D lands its half, R may call it, and not otherwise.
- `src/main.rs` and `src/collect/mod.rs` are R's, so stream E's filed request
  -- *a held request that does not resolve is a layer this machine cannot
  serve, not a failed run* (`collect::Unserved`, `run_all_within_unserved`) --
  is R's too.

**The coordinator owns** `examples/README.md`, `GUIDE.md`, `STATE.md`,
`README.md`, `DECISIONS.md`, `ROADMAP.md`, `CATALOG.md` and the `Makefile`.
The session-index test will be red on your branch for the sessions you add.
**That is expected** -- list the rows in your handoff. A decision worth
recording goes in your handoff under "For `DECISIONS.md`"; the coordinator
writes it.

---

## Stream R -- the flight recorder

> **Mission.** `STATE.md`, "What remains", item 1, and `DECISIONS.md`'s usage
> target: *a session whose duration is until a predicate fires, whose buffers
> are the bounded rings and maps it already has, and whose output is one
> document per trigger.* "A trigger and a retention bound, not a new kind of
> program."

What exists: the runner (`run_all_within`) runs rounds, detaches and attaches
requests inside a run (hot attach), and asks a trait between rounds
(`collect::Escalate`). What does not: a run with no fixed end, a retention
bound on what it keeps, a predicate evaluated over what a round read, and a
document for the retained window at each firing.

**The mechanism is yours, and it is the decision to write down.** Constraints
that come from what is settled:

- **One document covers one window**: the span the retention held when the
  trigger fired. `Results::window` exists; a triggered document carries its
  trigger, the predicate that fired, the value that fired it, and the window.
- **Eviction is a loss, counted and surfaced**, the way the join engine's
  expiry is. A retained distribution that dropped its oldest rounds says so
  through `Shortfall`, never silently.
- **One clock.** The trigger's timestamp is `CLOCK_MONOTONIC`.
- **The predicate uses the vocabulary that exists**: `src/filter.rs`'s grammar
  over a request's own emitted values (V owns `filter.rs`; if it needs an
  addition, file it). An invalid trigger is refused before the launch, with
  the reason.
- **The budget still holds** over a run with no end: the same `--budget`
  check, over a rolling interval.
- **A bound on how many documents**, and on how long the run lasts at most;
  both refused when absent or said in the resolved file when defaulted.

Also yours: stream E's `Unserved` request (above), and `--window` on the
command line if it is not done.

**Ground truth**: a workload that does something identifiable at a moment it
chooses and reports on its own clock -- a burst of known size after N seconds
of idling, twice -- and the recorder writes exactly two documents, each window
containing the burst, the retention bound respected, and the evicted rounds
counted against what the workload says it did before them. A trigger that
never fires ends at the maximum duration with zero documents and says so.

Handoff: `docs/recorder.md`.

---

## Stream V -- the probes tell the truth

> **Mission.** Stream P ran the tool on four kernels and five configurations,
> and the one invariant that failed is *available means it attaches*: ten
> classes, P1 to P10 in `docs/portability.md`, each filed under "the eleventh
> wave, stream P" in `docs/open-findings.md` with what is wanted. A probe that
> answers available and an attach that then refuses is the defect finding 15
> named, on a kernel nobody had tried.

1. **P1 to P10**, each as filed: the tracepoint prototype checked in the
   probe (P1), the BTF field a program reads checked by the probe rather than
   at attach (P2), the `uprobe` requirement below 6.6 (P3), the USDT backends'
   BTF requirement (P4), `net.request` below 6.6 (P5), a GPU library without
   its driver (P6), a collector that refuses a mode the resolver chose (P7),
   `system: native` refused (P8 -- the `cpu.profile` and `page_faults` half;
   the `sched` half is D's, file it to D if you find it is theirs), a
   per-task probe for a per-cpu event (P9), two requirements that name the
   wrong thing (P10). `KNOWN_DEFECTS` in `tests/portability.rs` fails when an
   entry stops matching: remove each entry as you fix it.
2. **Where a predicate actually runs** (stream S's request to
   `src/catalog.rs`): the catalog says per backend which fields the program
   filters in the kernel, and `resolve::placement` reports `Kernel` only for a
   predicate over those.
3. **The suite passes without capabilities**: the five tests stream P listed
   ("To the owners of five tests") assert the privileged answer where they
   should skip loudly or accept the unprivileged one -- four are in your
   files; `scenario_mem.rs`'s is D's. And `tests/vertical.rs`'s skip reason
   prints the first stderr line instead of the refusal.

**Ground truth**: the matrix itself. Rerun `scripts/matrix.sh` on the cells a
fix touches (the VM cache is under `target/matrix/`; mind the disk, about
9 GB free) and the cell goes from failing to passing, with the probe now
answering missing *with the reason the attach used to give*. A fix that makes
the probe say missing on 7.2 where the source attaches is a regression, and
the 7.2 cell is what catches it.

**You touch the catalog loader and the resolver**, which are shared layers:
one full `make check` before each commit is the rule anyway, and the capability
sweep in every `scenario_*` file is what a wrong probe breaks.

Handoff: `docs/portability.md`, a section "The twelfth wave".

---

## Stream D -- the kernel's attributions and drains

> **Mission.** What the eleventh wave found in kernel programs no stream
> owned, each a place where the tool credits an event to the wrong task or
> loses one between the program and the drain.

1. **Finding 48**: a wakeup performed from a hard or soft interrupt is
   credited to whatever task the interrupt landed on. Read the preempt count
   (`__preempt_count` through `bpf_this_cpu_ptr` of its ksym on x86), and
   under `HARDIRQ_MASK | SOFTIRQ_MASK` record the waker as the interrupt and
   keep the event only when its wakee is the target's.
2. **The same drain race in three more programs**: `io_pagecache.rs`,
   `net_qdisc.rs`, `net_softirq.rs` onto `map::Rows`, as stream K filed it,
   each with the `assert_rows_taken_once` test and its stack-region entry.
3. **`fentry::HistogramProgram`'s last drain sleeps 1 ms as its grace
   period**, which a preemptible kernel does not honour: `membarrier` or
   `Rows`.
4. **`sched.switches` follows threads**, on `perf::ThreadSampler` (stream S's
   request, two lines), and the `sched` half of P8 if it is yours.
5. **`mem.latency` with `-e stack` fails one run in ten** with
   `latency_cycles is not a numeric attribute` -- an IBS op with no latency
   made into something other than a number. Find what, and either emit a
   number with its meaning or leave the attribute out; and the capability-less
   test in `scenario_mem.rs` stream P listed.

**Ground truth**: for 1, the half-blocked workload beside a load that raises
interrupts (loopback datagrams, direct I/O) -- the reproduction in finding 48
-- gives the pair's wakeups and none credited to the target that it did not
make, over several runs. For 2 and 3, a known number of events drained on a
short tick, every one in a row or a counted loss, as K measured. For 4, a
program with N threads each switching a known number of times.

Handoff: `docs/families/kernel.md`.

---

## Stream M -- a model reads it

> **Mission.** `ROADMAP.md` phase 11: *an MCP server exposing what already
> exists as resources -- the resolved file, the results document, the
> catalog -- rather than a report generator. Nothing new is measured and
> nothing new is modelled.*

A new workspace member, `mcp/`, building a binary `omn-mcp` over the `omn`
library, speaking MCP over stdio. Your files are new, so nothing you do can
conflict; nothing in `src/` is yours, and if the library lacks an entry point
you need, file it and read the file formats instead.

**The two constraints from the roadmap are the design:**

- **It reads; it does not run** -- boundary 5. A model may *ask* for a
  measurement: a tool that validates and resolves a session and says what it
  would cost and what it would degrade, and at most one that runs a session
  through the `omn` binary with a bounded duration and a budget. It may never
  change the machine. Decide whether the running tool exists at all, and write
  down why.
- **Every number it is handed carries what it is missing.** `Shortfall`,
  losses, the evidence level of every attribution, `ts_err_ns`, the
  degradations of the resolved file: a tool's answer that drops one of those
  is the defect this phase exists not to have. `IDENTITY` and `TEMPORAL` must
  not read alike.

Resources: the catalog (families, sources, backends, caveats), a run's
resolved file and results document from a directory the server is pointed at,
`omn list`'s capability sweep for this machine. Keep dependencies to what the
workspace already has (`serde_json` is enough for JSON-RPC over stdio);
a new crate is a decision to justify in the handoff.

**Ground truth**: real documents -- run the example sessions with `omn -f`,
point the server at the output, and check that every number, loss, shortfall
and evidence level in the result files is present, unchanged, in what the
server returns; and that a lossy run's answer carries its shortfall. A tool
call that would change the machine is refused. Test with a small client in
`mcp/tests/` that speaks the protocol to the binary.

Handoff: `docs/mcp.md`.

---

## Rules

The three that have held for eleven waves:

1. **The unit of ownership is a file set nobody else writes.** The table above.
2. **The handoff is a file only that stream can write**: R's is
   `docs/recorder.md`, V's `docs/portability.md`, D's
   `docs/families/kernel.md`, M's `docs/mcp.md`. End it with **the index rows
   for every session you added**, **what is for `DECISIONS.md`**, and
   **anything you filed for another stream**.
3. **A task is landed when its scenarios are**: ground truth, the capability
   sweep, the loss path, the refusal.

**Build with `make`, not `cargo build`** -- the preload shim and the
capabilities both fail silently otherwise. One branch per stream, made by
`scripts/spawn.sh`. Commit there; the coordinator merges.
