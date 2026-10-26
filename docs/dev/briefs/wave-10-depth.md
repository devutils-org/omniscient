# Brief — depth: hot attach, the network rows, the bench, and triage

Four work streams. The tenth fan-out, and the first with no source to add:
`docs/dev/STATE.md` says breadth is no longer where the work is, and the five sources
left wait on hardware this machine does not have. What is left is depth — the
last task of three roadmap phases, and the first of the layer above the engine.

Read `CLAUDE.md` first — **including "How much to test, and when", which this
wave is held to** — then `docs/dev/STATE.md`, then your section here, then
`docs/contributing.md`. The nine earlier briefs are in `docs/dev/briefs/`.

**This is the only brief.** There is no `BRIEF_NEXT.md` any more: what remains
after this wave is in `docs/dev/STATE.md` under "What remains", and nowhere else. A plan
written in two places drifted twice, so it is written in one.

---

## Before anything else: how you test

The full suite is about 1620 tests and takes 5 to 10 minutes, because some 300
scenarios measure the real machine. The ninth wave was the first held to this
rule and the first whose merge went green on its first run. Same rule:

- **While you work**: run only your own module and your own test file —
  `cargo test --lib <module>` and `cargo test --test <your file>`. Never
  `make check` while iterating.
- **Before each commit**: one `make check`. If it is red, run
  `cargo test --workspace --no-fail-fast` once to see every failure together.
- **A red test outside your paths**: look it up in the flaky tally
  (`docs/open-findings.md` findings 41–46, `docs/scenarios/io.md`). If it is
  there, rerun it alone once and move on. Do not rerun the suite for it, and do
  not try to fix it — it is not yours.
- **`make`** once per build, before eBPF tests, because a relink drops the
  capabilities. Not once per test.

---

## What changed before this wave

**Phase 4 is done within what needs no root.** One request is decomposed
boundary by boundary on a real server, every row at `IDENTITY`, summing to the
client's own latency within +0.11 % to +0.42 %. The two rows the roadmap's tree
draws and the tool does not are `net_rx` and `net_tx`: on loopback they are the
24 to 60 µs between the client's latency and the request's life, and nothing
names them yet.

**Three phases are one task short each**, and each of those tasks is its own
stream:

| Phase | What is left | Stream |
|---|---|---|
| 4 — the vertical case | the two network rows | **N** |
| 6 — emit, aggregation, budget | hot attach and detach | **H** |
| 8 — self-benchmark | every backend of a chain, measured against the others | **X** |

**And phase 10 has its first method and nothing above it.** TSA is what
`examples/join-decompose-compute.yml` produces. The objective, USE, the
question-oriented modes and differential flame graphs are stream **T**. The
escalation is not: it needs hot attach, and it is the next wave's.

---

## The streams

| | Stream | Delivers | Owns |
|---|---|---|---|
| **H** | hot attach | a collector added and removed mid-session, without breaking the run or its joins | `src/collect/mod.rs`, `src/collect/launch.rs`, `src/collect/task.rs`, `src/session.rs`, `src/request.rs`, `tests/runner.rs`, `tests/budget.rs`, `examples/attach-*.yml`, `docs/scenarios/attach.md` (new) |
| **N** | the network rows | `net_rx` and `net_tx` in the vertical tree, at their evidence level | `src/bpf/request*.rs`, `src/bpf/net_*.rs`, `src/collect/net/**`, `catalog/net.yaml`, `examples/vertical/**`, `examples/vertical-*.yml`, `examples/net-*.yml`, `tests/vertical.rs`, `tests/scenario_net.rs`, `docs/scenarios/vertical.md`, `docs/families/net.md` |
| **X** | the bench | phase 8's harness, its variance gate, and the backend comparison | `tests/bench_*.rs` (new), `examples/bench/**` (new), `examples/bench-*.yml`, `scripts/bench.sh` (new), `src/capabilities.rs`, `docs/bench.md` (new) |
| **T** | triage | the objective, USE, `omn latency` and `omn saturate`, differential flame graphs | `src/triage/**` (new), `src/cli.rs`, `src/main.rs`, `src/lib.rs`, `tests/triage.rs` (new), `examples/triage-*.yml`, `man/omn.1`, `docs/triage.md` (new) |

**Nobody owns** `src/join.rs`, `src/emit.rs`, `src/encode.rs`, `src/render.rs`,
`src/resolve.rs`, `src/validate.rs`, `src/catalog.rs`, `src/bpf/btf.rs`,
`insn.rs`, `map.rs`, `syscall.rs`, `attach.rs`, `usdt.rs`, `src/bpf/mod.rs`, or
any `catalog/*.yaml` but `net.yaml`. A stream that needs one writes the request
under `## Needs from another stream` in `docs/open-findings.md`, **under a
heading naming the stream**, with the exact signature, and carries on. That file
is append-only per stream.

**Two seams to name before they are discovered.** `src/bpf/mod.rs` is where a
new program registers: N will need a line there if it adds a file, and it goes
in the handoff, not in the edit. And `src/lib.rs` is T's, because T is the only
stream adding a module; a stream that needs a `mod` line asks T's section of
the findings file.

**The coordinator owns** `examples/README.md`, `docs/GUIDE.md`, `docs/dev/STATE.md`,
`README.md`, `docs/DECISIONS.md`, `docs/ROADMAP.md`, `docs/CATALOG.md` and the shared files
above, and is working in them on `main` while the streams run: the ninth wave's
unanswered requests (a cycle unit, an event whose value was not measured, a loss
reason beside clean counters, `map_update` outside `#[cfg(test)]`, the page
cache's map moved onto task storage, `drained: true` across the catalogs). None
of it is in a file a stream owns. The session-index test will be red on your
branch for the sessions you add. **That is expected** — list the rows in your
handoff and the coordinator adds them at the merge.

---

## Stream H — hot attach

> **Mission.** `docs/ROADMAP.md` phase 6, task 4: *a collector can be added and
> removed mid-session*, without restarting the run or breaking in-flight joins.

`docs/DECISIONS.md` settled where this lives in the ninth wave: **inside a session,
with no daemon.** The run loop is `run_all_within` in `src/collect/mod.rs`, and
today every request in a session shares one window, attached before the launch
gate opens and stopped at the end. What you build is a request whose window is
**its own and inside the run's**: attached at a moment, detached at another,
while the rest keep running.

**The surface is yours to design, and it is the decision worth writing down.**
The session file is where it starts — a per-request start and stop, relative to
the run — and the CLI can follow. Whatever you choose, the dumped request has to
read back unchanged, because phase 7's invariant is that the file and the CLI
build the same `Request`.

**What must not break, and each is a scenario:**

- **Every request's document says its own window.** A source attached for two
  seconds of a ten-second run has a rate over two seconds, not ten. If the emit
  layer cannot say that, it is a request to the coordinator, not an edit.
- **A join across a detached source is not a loss.** Events of the other side
  that fall after the detach fell outside that source's window, not outside
  every open range. Counting them as unattributed is the silent degradation
  this project is built against; `src/join.rs` is not yours, so file the exact
  field it needs.
- **Attach costs are counted.** A program loaded mid-run is loaded while the
  target runs; the overhead a run measures must include it, and the losses of
  the collector's first tick are surfaced like every other loss.
- **A source that cannot be attached late says so.** An `LD_PRELOAD` collector
  exists only at exec; `Prepared::requires_launch` already knows. Refuse, by
  name, before the run starts.

**Ground truth**: a program with two phases the test knows the boundary of — a
cpu-bound second and a sleeping second, say — observed by one source attached
only for the first. Its count must match the first phase and nothing of the
second; a second source attached for the whole run must match the whole.

The concurrent-requests line in `docs/DECISIONS.md` (*"a session with several is
refused… concurrent attach is the honest fix, and it lands with hot attach"*)
is yours to make true or to say why not. The escalation of phase 10 is **not**
in this wave: it is the next one, and it will stand on what you build.

---

## Stream N — the network rows

> **Mission.** The two rows of the roadmap's tree the tool does not draw yet:
> `net_rx` and `net_tx`, each at the evidence level it has earned.

`docs/scenarios/vertical.md` already says what is missing and where it goes:
*a row for them needs the connection's cookie at the request's birth and end —
`bpf_get_socket_cookie` on the socket the server received on, which the birth
probe would have to be given as an argument or read from the thread's last
receive. It is a second argument on the same two probes.* That is the design
the ninth wave left you; take it or say why not.

`net_rx` is from the request's bytes reaching the socket to the serving thread
taking them; `net_tx` from the response leaving the thread to it leaving the
stack. Both are `KEY` by the roadmap's table — a `sock:cookie` — and the
ladder must say so: a row read by the cookie is not `IDENTITY` because its
neighbours are. `SO_TIMESTAMPING`'s software stamps are available on loopback
without a NIC and without root.

**The ground truth needs no root, and that is the interesting part.** `tc
netem` would inject a delay the tool never sees happen; a client can inject one
the tool does see. A client that sends half its request, sleeps a known 30 ms,
and sends the rest puts 30 ms in `net_rx` if the request is born at its first
byte. A client with a small receive buffer that reads the response slowly puts
a known wait in `net_tx`, because the server's send blocks on the window. Say
in `docs/scenarios/vertical.md` which of the two the tool now holds, and keep
the existing criterion: **the rows still sum to the client's latency within 10
%**, now with two more of them.

Three limits the ninth wave recorded, yours if the rows leave time: probes are
looked for in the executable only; a probe behind a semaphore is refused; a
request handed to another thread is not tested. Each is a line in
`docs/scenarios/vertical.md` today. `drained: true` on `catalog/net.yaml`'s
kernel-aggregated backends is also yours — stream J asked for it on every
catalog in the eighth wave, and `net.yaml` is the one the coordinator cannot
reach this wave.

---

## Stream X — the bench

> **Mission.** `docs/ROADMAP.md` phase 8: a cost, resolution and loss table measured
> for every (source, backend) this machine can serve, and the resolver's
> choices confirmed by it or corrected.

What exists: a run measures its own cost and writes `cost.json` beside the
results, and `--backend force:<id>` pins a backend — the scenario wave uses it
for agreement between backends, never for their price. What does not: the
harness around them.

**The harness is the phase's task list, in order**: reference workloads
(launch-bound, compute-bound, memory-bound, syscall-heavy, IO-heavy, contended
multithread), a warmup, **at least 20 repetitions round-robin between methods**,
median and IQR. Then the backend comparison, the effective resolution sweep (two
events X µs apart, X from 0.5 to 50), and the observer effect. The roadmap's
caches paragraph applies: evict the LLC between repetitions, and a cold page
cache is `POSIX_FADV_DONTNEED` rather than `drop_caches`, which needs root.

**The variance gate comes first, and it will refuse on this machine while the
other three streams build.** The roadmap is explicit: *if the baseline's CV is
over 1 %, the bench cannot resolve a 1–3 % overhead — fix the environment before
measuring anything.* That refusal is the gate working, not a failure: write it,
prove it refuses under load, and do **not** produce a table from a noisy
machine. The real measurement is run once, alone, after the merge — the handoff
says how, in one command.

**The bench is not part of `make check`.** Its tests are `#[ignore]` and run
through `scripts/bench.sh`; a suite that measures for twenty minutes is the one
thing this wave's testing rule exists to prevent. What does run in the suite is
cheap: the statistics on synthetic samples, the gate's refusal on a series
whose CV is known, the round-robin order.

**`src/capabilities.rs` is yours** because the environment capture lives there
and phase 8 wants the prefetcher state in it — *captured, not controlled*, per
boundary 5. Stream S's request for a `dynamic-linking` probe is in the same file
and is yours to answer.

---

## Stream T — triage

> **Mission.** `docs/ROADMAP.md` phase 10, everything but the escalation: the
> objective as a parameter, USE, the question-oriented modes, and differential
> flame graphs.

**The layer sits above the engine and calls it like any other user.** The
roadmap says so, and it is also the ownership: `src/triage/` builds requests,
runs them through the same library calls `main.rs` makes, and reads the
results. It does not edit the collector layer, the join engine or the emit
layer. Needing something from them is a filed request.

**The objective comes first** (phase 10, item 0): median latency, the tail,
throughput and energy do not name the same culprit, and a triage answer names
the objective it answered for. A request that does not say which is refused
with the list, not defaulted.

**Then the methods**, in the order their inputs exist:

- **`omn latency`** — *why is this slow*: TSA as a question. The decomposition
  already exists (`examples/join-decompose-compute.yml`); this is the mode that
  asks for it and says what dominates, with the residue and the evidence level
  of every row carried through rather than summarised away.
- **`omn saturate`** — USE: utilization, saturation and errors per resource,
  from sources that already collect. Little's law is the arithmetic
  (`io.inflight` against `io.iops`, run queue length against `sched.runqlat`),
  and a percentile from a lossy run carries its `Shortfall` — finding 37 exists
  for exactly the reader this layer is.
- **Differential flame graphs** — two profiles and a subtraction; the `folded`
  and `speedscope` encoders already exist.

**Ground truth**: three workloads whose limit the test chose — cpu-bound,
blocked on I/O, contended on a lock — each named correctly for the objective
asked, and one workload whose answer changes with the objective. For the
differential graph, one program run twice with one function's cost doubled:
that function, and only it, is the difference.

`man/omn.1` is yours this wave because the new commands are; the eighth wave's
request for `between` and `decompose:` in the manual's session section is
yours to answer while you are there.

---

## Rules

The three that have held for nine waves:

1. **The unit of ownership is a file set nobody else writes.** The table above.
2. **The handoff is a file only that stream can write**: H's is
   `docs/scenarios/attach.md`, N's `docs/scenarios/vertical.md`, X's
   `docs/bench.md`, T's `docs/triage.md`. End it with **the index rows for every
   session you added**, ready to paste.
3. **A task is landed when its scenarios are**: ground truth, the capability
   sweep, the loss path, the refusal. For X, the gate refusing on a noisy
   machine is one of them.

**Build with `make`, not `cargo build`** — the preload shim and the capabilities
both fail silently otherwise. One branch per stream, made by `scripts/spawn.sh`.
Commit there; the coordinator merges.
