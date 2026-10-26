# Brief — a machine that is not the developer's

Six work streams. The fourteenth fan-out, and the first **product wave**:
every roadmap phase is done bar three filed edges, and what is missing is what
someone who is not the developer meets first. The CI's second run found it:
on a stock kernel, an unprivileged user cannot profile their own program, and
on a cloud VM the tool calls a hardware counter available that is not there.
This wave makes the tool right on that machine, installable, and demonstrable
end to end. It also lands what the thirteenth wave filed, grouped by the files
it lands in.

Read `CLAUDE.md` first -- **including "How much to test, and when", which this
wave is held to** -- then `docs/dev/STATE.md`, then your section here, then
`docs/contributing.md`. The thirteen earlier briefs are in `docs/dev/briefs/`.

**This is the only brief.** What comes after this wave is written in
`docs/dev/STATE.md` under "What remains", and nowhere else.

---

## Before anything else: how you test, and how you read

The full suite is about 2000 tests and takes 5 to 10 minutes. The rule:

- **While you work**: run only your own module and your own test files --
  `cargo test --lib <module>` and `cargo test --test <your file>`. Never
  `make check` while iterating.
- **Before each commit**: one `make check`. If it is red, run
  `cargo test --workspace --no-fail-fast` once to see every failure together.
- **A red test outside your paths**: look it up in `docs/flaky.md` and the
  "for the flaky tally" notes of `docs/open-findings.md`. If it is there, rerun
  it alone once and move on. Do not rerun the suite for it, and do not try to
  fix it -- it is not yours.
- **`make`** once per build, before eBPF tests, because a relink drops the
  capabilities. And **run `cargo fmt` before `make`, never after it**.
- **Read by range**: `grep -n`, then the lines you need. Never a large file
  whole. Filter test output to the result lines and the failures.
- **The portability matrix is the coordinator's, at the end of the wave.** Do
  not start `scripts/matrix.sh`; list in your handoff the cells your change
  should move, with the command.

**The machine you are fixing for is not this one.** The development machine
runs at `perf_event_paranoid` -1, with a hardware PMU and every privilege this
tool asks for, and it cannot be put at paranoid 2 or 4 without root. The two CI
runs are the ground truth for what a stock machine does, and they are kept
outside the repository, in `/home/moha/repos/omniscient-ci-logs/` (its
`README.md` says which run is which): `run1-paranoid4/` is the hosted runner
as shipped (paranoid 4, no user namespaces), `run2-paranoid2/` the kernel's
defaults (paranoid 2). Each holds the Actions log, the suite's log and the
runner's matrix cell. A fix for a stock machine is checked by the next push's
CI, which the coordinator makes and reads back to you.

Six streams and a coordinator share this machine. Assert the separation, not
a threshold calibrated on a quiet machine.

---

## What changed before this wave

**The thirteenth wave landed all five of its streams**, and
`docs/dev/briefs/wave-13-cost.md` keeps its brief. What they filed is in
`docs/open-findings.md` under "the thirteenth wave", one heading per stream,
plus the coordinator's two at the end: the read-ahead scenario's one lookup in
256, and **"The CI's second run: two defects a developer's machine cannot
show"**, which is this wave's first item.

**`docs/open-requests.md` is a snapshot taken before the twelfth wave.** An
item this brief cites from it is to be checked against the code first; one
that has landed is a line in your handoff, not work.

---

## The streams

| | Stream | Delivers | Owns |
|---|---|---|---|
| **P** | the stock machine | a probe that opens what it promises, the CI green on both cells, and the validator's filed checks | `src/capabilities.rs`, `src/resolve.rs`, `src/validate.rs`, `src/filter.rs` (except `check_stack`, U's), `src/collect/cpu/**` except `profile.rs` (and the lines of `cpu/mod.rs` that read `request.stack`, U's), `src/collect/mem/page_faults.rs`, `src/collect/power/**`, `src/collect/gpu/**`, `src/collect/runtime/**`, `src/bpf/usdt.rs`, the `cpu.*` (except `cpu.profile`), `power.*`, `gpu.*`, `runtime.*` and `mem.faults` entries of `catalog/*.yaml`, `tests/scenario_{cpu,power,gpu,runtime}.rs`, `tests/family_{cpu,power,gpu,runtime}.rs`, `tests/portability.rs`, `tests/catalog_model.rs`, `scripts/matrix.sh`, `scripts/vm/**`, `.github/**` except `workflows/release.yml`, `examples/{cpu,power,gpu,runtime}-*.yml`, `docs/portability.md` |
| **R** | the results say it | the vocabulary three streams are waiting on, a window that ends where its collector stopped, and what the engine knows printed | `src/collect/mod.rs`, `src/collect/launch.rs`, `src/collect/task.rs`, `src/main.rs`, `src/session.rs`, `src/request.rs` (except `StackMode`, U's), `src/cli.rs`, `src/catalog.rs`, `src/emit.rs`, `src/encode.rs`, `src/render.rs`, `src/triage/**`, `src/bpf/syscall.rs`, `shim/**`, `src/collect/mem/heap/**`, `tests/{budget,runner,triage,recorder,encoders,aggregation,cli}.rs`, `tests/bench_*.rs`, `examples/bench/**`, `examples/{record,triage}-*.yml`, `man/omn.1`, `docs/fixes/results.md` (new) |
| **K** | the kernel programs | the 6.17 prototypes, the scenarios that must skip on a stock machine, and the read-ahead lookup | `src/bpf/**` except `syscall.rs` and `usdt.rs`, `src/collect/{sched,proc,io,net}/**`, `src/collect/mem/**` except `page_faults.rs` and `heap/**`, `src/collect/psi.rs`, the `sched.*`, `proc.*`, `io.*`, `net.*` and `mem.*` entries of `catalog/*.yaml` except `mem.faults`, `tests/scenario_{sched,proc,io,net,mem}.rs`, `tests/family_{sched,proc,io,net,mem}.rs`, `tests/raw_tracepoints.rs`, `examples/{sched,proc,io,net,mem}-*.yml` except `sched-wakeup.yml`, `docs/families/kernel.md` |
| **J** | an agent benchmarks a program | the `omn-mcp` use case, end to end, and the engine's last filed edges | `src/join.rs`, `src/event.rs`, `src/clock.rs`, `mcp/**`, `tests/join_engine.rs`, `tests/cross_*.rs`, `tests/vertical.rs`, `examples/{cross,join,vertical}-*.yml`, `examples/sched-wakeup.yml`, `examples/mcp/**` (new), `docs/mcp.md`, `docs/families/join.md` |
| **D** | it installs | a release someone else can install and run, and the first ten minutes of reading | the `Makefile`, `scripts/release.sh` (new), `.github/workflows/release.yml` (new), `README.md`, `docs/DEMO.md`, `docs/install.md` (new) |
| **U** | every way to unwind a stack | unwind modes as a chain the resolver chooses from, like backends: frame pointers, frame pointers with the leaf's caller recovered, DWARF, LBR, SFrame; and names from debuginfod | `src/perf.rs`, `src/stack.rs`, `src/unwind.rs` (new), `catalog/unwind.yaml` (new), `src/collect/cpu/profile.rs`, the `cpu.profile` entry of `catalog/cpu.yaml`, `StackMode` in `src/request.rs` and its parsing, `check_stack` in `src/validate.rs`, the lines of `src/collect/cpu/mod.rs` and `src/collect/mem/mod.rs` that read `request.stack`, `tests/stack_capture.rs`, `tests/unwind.rs` (new), `examples/unwind-*.yml` (new), `docs/unwind.md` (new) |

**A catalog entry belongs to the stream that owns its collector**, whole. Its
`cost:` keys are no longer split off. `docs/CATALOG.md` is the coordinator's.

**Nobody owns** `src/error.rs`, `src/lib.rs` and the tests not listed. A stream
that needs one writes the request under `## Needs from another stream` in
`docs/open-findings.md`, **under a heading naming the stream and the
fourteenth wave**, with the exact signature, and carries on. That file is
append-only per stream.

**Three splits to respect.**

- **R lands its vocabulary first, in its first commit, and says so.**
  `Unit::Watts`, `KeyKind::Node` and `KeyNotCarried` are each an enum variant
  that R's exhaustive matches must take in the same commit. P (`Unit::Watts`,
  `KeyNotCarried`) and K (`KeyKind::Node` for `mem.numa`) wait on them. The
  coordinator merges that commit to `main` as soon as it exists, and tells P
  and K to rebase.
- `src/validate.rs` is P's, and the two checks the engine's owners asked of it
  (a decomposition against the session's joins, and an attribute a ranking
  names) are P's; `check_stack` in it is U's.
- **U's first commit is the one P, K and R build against** where they meet a
  stack: the `StackMode` values and the `stack::` entry point that chooses a
  mode. The coordinator merges it to `main` as soon as it exists. The call sites in `main.rs` and `triage/engine.rs` are R's.
- `README.md` and `docs/DEMO.md` are D's this wave. `docs/GUIDE.md`, `docs/dev/STATE.md`,
  `docs/DECISIONS.md`, `docs/ROADMAP.md`, `docs/CATALOG.md` and the two session indexes stay
  the coordinator's.

**The coordinator owns** `examples/README.md`, `docs/GUIDE.md`, `docs/dev/STATE.md`,
`docs/DECISIONS.md`, `docs/ROADMAP.md`, `docs/CATALOG.md`, `docs/bpf-layer.md`,
`docs/open-requests.md` and `scripts/spawn.sh`. The session-index test will be
red on your branch for the sessions you add. **That is expected** -- list the
rows in your handoff. A decision worth recording goes in your handoff under
"For `docs/DECISIONS.md`".

---

## Stream P -- the stock machine

> **Mission.** The CI's second run (`docs/open-findings.md`, "The CI's second
> run"): on a cpu with no hardware PMU, `cpu.cycles` reads available and is
> refused at the attach, and the suite fails where it should skip on a stock
> machine. Neither can be seen on the development machine. (Its other defect,
> `cpu.profile` refused at paranoid 2, is stream U's: it is a callchain.)

1. **A probe that opens what it promises.** `cpu.cycles`, and every source whose
   requirement is a hardware event, reads available on a cpu that publishes no
   such event (`ENOENT` at the open). The probe opens the event, as `fentry`'s
   does, and answers missing with the reason. A cloud VM is the machine many
   users have.
2. **The CI green, on two cells.** The one P configured (paranoid 2), and a
   second job at the hosted image as shipped (paranoid 4, no user
   namespaces), where every perf test must skip with the machine's reason
   rather than fail. What you find failing in another stream's files is
   filed, one request per owner, with the log line. The skip gate's ten
   unexpected skips of the second run are yours to sort.
3. **What the thirteenth wave filed against your files**, each checked first:
   `check_decomposition_in` against the session's joins (J), an attribute a
   ranking names is one the entry declares (R), `SCHED_SWITCHES`'s
   `dead_code` (S), `Unit::Watts` and `gpu.power` in watts and `KeyNotCarried`
   once R's vocabulary is on `main`, and the multi-size THP caveat on
   `mem.faults` (N's text, filed).

**Ground truth**: for 1, the next push's CI (the coordinator pushes when you
commit and reads the run back to you): `cpu.cycles` missing, by name, on the
Azure runner, and on this machine unchanged. For 2, both CI cells green.

You touch the probes, the resolver and the validator, shared layers: a full
`make check` before each commit.

Handoff: `docs/portability.md`, a section "The fourteenth wave".

---

## Stream R -- the results say it

> **Mission.** Three streams filed enum variants that only R's exhaustive
> matches can take, and two filed things the engine knows that no rendering
> prints. Land the vocabulary first, then the rest.

1. **The vocabulary, first commit**: `Unit::Watts` (P's `gpu.power`),
   `KeyKind::Node` and `KeyValue::Node` (J's patch, filed; K's `mem.numa`),
   `ResolveError::KeyNotCarried` (P, `open-requests 2539`). Each with the arms
   in `main.rs` and `encode.rs` (`pprof_unit`, the text units). Say in your
   first report that it is committed, with its id.
2. **A window ends where its collector stopped** (stream S): `closed_ns` set
   per collector just before its `stop()`, so a result's window never claims a
   span no row could hold. Ground truth: S measured 6 of 930 blocks outside
   every row with `sched.offcpu` alone, 71 with five collectors.
3. **What the engine knows, printed**: the decomposition's call sites take the
   session's joins (`decompose_with`, J's request); the text renderer prints
   the `overlap` and `woke` amounts and a CHAIN column wide enough for two hops;
   the evidence ladder's sentences (`Evidence::establishes`) beside a join's
   level.
4. **A container resolved to its cgroup** (stream S, signature filed), so
   `--container` stops being refused wherever a cgroup is served.
5. **The smaller ones**, each checked first: `pub mod psi;` where it lives (N);
   `Collector::facts` for `net.softirq`'s note, done with K; the flaky tally's
   #20 line in `emit.rs`; one `parse_cpu_list` (`syscall.rs:593`, the one in
   `capabilities.rs` stays); two parallel tests sharing one bench report file
   (`examples/bench/harness.rs`); `Cost::Extreme`'s doc to what the measured
   backends are (R, thirteenth wave); and the paranoid-4 failures of the CI's
   first run in `bench_capabilities.rs`, `encoders.rs`, `aggregation.rs`,
   `triage.rs`, `runner.rs` and `collect::tests`, each skipping with the
   machine's reason.

**Ground truth**: for 2, S's measurement against the kernel's own `ru_nvcsw`;
for 3, a session whose joins define a part, rendered, against its JSON; for 4,
a process in a named cgroup reached by `--container` the way the resolver
would reach a container runtime's.

You touch the runner, the emit layer and the catalog's types, shared layers: a
full `make check` before each commit.

Handoff: `docs/fixes/results.md`.

---

## Stream K -- the kernel programs

> **Mission.** The stock machine again, from the kernel side: two prototypes
> 6.17 moved, scenarios that fail on a runner where they should skip, and one
> lookup in 256 that may have been lost without a word.

1. **6.17's prototypes**: `tcp_v4_connect` (`fentry.rs:1523`, a `sockaddr_unsized *`)
   and the `mem_stall` tracepoint (`gfp_t, int, mem_cgroup *`), each a second
   shape in the program and its requirement, in the same commit.
2. **The read-ahead lookup** (`docs/open-findings.md`, the coordinator's
   thirteenth-wave note): `scenario_io::a_sequential_read_is_read_ahead_of...`
   counted 255 of 256 three times, only inside the whole `scenario_io` run.
   First make the test print the run's `losses` and the lookups per inode when
   the count is off; then decide whether the loss was counted (the test carries
   it as slack) or silent (`io_pagecache.rs`'s drain, a defect).
3. **What the runner showed** (P's thirteenth-wave filings to S and N), each
   checked first: `proc::status`'s first-reading race; `scenario_proc`'s fd
   count off by the two descriptors the runner passes down; the scenarios that
   assume paranoid -1 (`scenario_sched`, `scenario_mem`); `scenario_io`'s
   eleven skips when the build directory is not on a block device (`/dev/root`);
   `net.hwts` listed usable with no PTP clock; `family_net`'s two panics where
   it should skip; `writable:` for `mem.wss`; `container: no` on
   `mem.cache_*`; `attach.rs`'s own `online_cpus`.
4. **The rest filed for S and N**: `net.softirq`'s note through `facts()` once
   R lands it; `mem.numa` keyed by node once R's `KeyKind::Node` is on
   `main`; the `BPF_ENABLE_STATS` re-export in `bpf/mod.rs` (R's request).
5. **`sched.switches` per cgroup**, the one open line in `docs/DECISIONS.md`: decide
   it and write down why. A `sched_switch` program first in the chain for every
   target changes what every existing `sched.switches` run costs.

**Ground truth**: for 1, the CI's next run on 6.17 (the coordinator reads it
back to you), and this machine unchanged; for 2, the lookups the workload
made, from its own `mincore`, against the drain's rows and its counted loss;
for 3, the CI logs in `omniscient-ci-logs/`, where each failure named becomes
a skip with the machine's reason.

You touch the eBPF layer, a shared layer: a full `make check` before each
commit.

Handoff: `docs/families/kernel.md`, a section "The fourteenth wave".

---

## Stream J -- an agent benchmarks a program

> **Mission.** `omn-mcp --allow-launch` exists since the thirteenth wave. Make
> the use case it was built for work end to end, for someone who has never
> read this repository: *"benchmark my program, and tell me what dominates its
> time"*, asked of an agent.

1. **The use case, end to end.** A directory of approved sessions under
   `examples/mcp/` (a launched program, a profile, a decomposition, the
   triage's cheap pass), a client configuration, and `docs/mcp.md`'s first
   section rewritten as that walk-through: start the server, ask, read what
   the agent is handed. What a model needs to compare two runs -- the spread of
   a repeated measurement, a shortfall, an evidence level -- is in what
   `run_session` returns, or the walk-through says where it is.
2. **Measured, not described**: a test in `mcp/tests/` that drives the
   walk-through's calls against the example directory and checks every
   number the agent is handed against the run's own files, and one that
   repeats a session N times through `run_session` and finds the spread the
   bench's gate would compute from the same runs.
3. **The engine's filed edges**: `overlap` over machine-wide state with no key
   (`Rule::via: Option`; the validator half is P's, file its exact shape), and
   `omn-mcp`'s cost band text for `extreme` (R's thirteenth-wave request).
4. **The paranoid-4 failures in your files** (the CI's first run):
   `cross_compute` (four tests), `cross_machine`, and `mcp/tests/protocol.rs`,
   each skipping with the machine's reason where `cpu.profile` is withheld.

**Ground truth**: for 1 and 2, the example directory run through the server
by the test, and every number and loss it hands over found unchanged in the
files; for 3, the workload of the crossing that asked.

You touch the join engine, a shared layer: a full `make check` before each
commit.

Handoff: `docs/families/join.md`, a section "The fourteenth wave", and
`docs/mcp.md`.

---

## Stream U -- every way to unwind a stack

> **Mission.** A profile's stacks are only as good as the way they were
> unwound, and today there is one way: the kernel's frame-pointer walk. It
> loses the caller of every function that sets up no frame (every libc leaf,
> most of a stock Python), and on the kernel's default paranoia it is refused
> outright. `docs/ROADMAP.md` said frame pointers first and DWARF later, with a
> library and never an unwinder of our own. This wave does the rest. The modes
> sit **side by side, chosen the way backends are**: declared as data, probed,
> ranked, forced with `--stack`, and each saying what it costs and what it
> loses.

1. **Unwind modes are data.** `catalog/unwind.yaml` declares each mode like a
   backend: its requirements (a probe answers available, missing or unknown),
   its cost (the bytes it copies per sample, measured), and what it degrades.
   `src/unwind.rs` loads it and chooses: `--stack auto`, the default, takes
   the best mode this machine and this target serve, and says why it passed
   over the others, in the resolved file as the resolver does for a backend.
   `--stack <mode>` forces one and is refused with the reason where it cannot
   be served. The same shape as backends, not a second mechanism beside them:
   reuse `resolve.rs`'s vocabulary for a choice and its rejections, and file
   what you need of it (P's).
2. **The modes**, each landed with its scenario or refused with its reason:
   - `fp`: the kernel's frame-pointer callchain, as today.
   - `fp+leaf`: `fp`, plus the caller of a frameless leaf recovered from the
     return address at the top of the stack. Ask for the user registers and a
     few words of the user stack with each sample; use the binary's CFI to tell
     where the return address is. A caller recovered this way is marked as
     such.
   - `dwarf`: the user stack copied with each sample and unwound in user space
     from `.eh_frame`, through a library (`framehop`, or libdw; the crate is a
     decision to justify). It serves a program built without frame pointers,
     at a cost measured and printed.
   - `lbr`: the branch stack, where the cpu has one (Intel; AMD from Zen 4's
     LbrExtV2). Probed, and missing by name on this Zen 2.
   - `sframe`: the kernel reading a binary's `.sframe` section, where the
     kernel and the binary both support it. Establish what upstream Linux
     supports and from which release, from the kernel source, before writing
     anything, and write it down. If no kernel this project runs on serves it,
     the mode is declared, probed and missing with that reason.
   - `none`.
3. **The kernel's default paranoia** (P's defect in the CI's second run):
   `cpu.profile` is refused at `perf_event_paranoid` 2 because the callchain
   asks for the kernel half. Confirm it, then serve user-only callchains and
   samples where the paranoia requires it, with the resolved file saying the
   stacks are user-only. The chooser in item 1 is where this decision lives.
4. **Names, from debuginfod**: `--symbols local|debuginfod`. A stripped
   binary's names (libc's internals, a distribution's `sqlite3`) are fetched
   by build id from the servers in `DEBUGINFOD_URLS`, cached, and never
   fetched unless asked, because it is a network request about what this
   machine runs. The resolved file says where each name came from. A frame
   still unnamed stays `lib+0xoffset`.
5. **The profile's drops per ring** (stream R's thirteenth-wave request): the
   plumbing is in `Collection`; `perf.rs` and `profile.rs` report which ring
   lost what. What it changes in `tests/cross_compute.rs` (J's) is a line in
   your handoff.
6. **What frames were lost, said by the mode that lost them.** The note
   *"N of M samples landed in a function that sets up no frame pointer ... this
   target was built without them"* says the target when it is libc; it names
   the object the frameless function is in, and under `fp+leaf` or `dwarf` it
   counts what was recovered instead.

**Ground truth**: a C program whose call tree is known by construction, built
four ways (frame pointers everywhere; frame pointers but frameless leaves, as
gcc makes them at `-O1` and above; no frame pointers; stripped), whose time
splits across its branches in known shares. Each mode is held to the tree it
should recover and the share of each branch, and to the cost it declares,
against the program's own `CLOCK_THREAD_CPUTIME_ID`. For 3, the CI's next run
at paranoid 2. For 4, a stripped build of that program, and its names against
the unstripped build's. Two such programs are in `/home/moha/repos/omniscient-ci-logs/`
(`calltree.c`, known shares by construction; `loganalyzer.c`, parse,
aggregate, rank and report over 1.5 M log lines), with how each was built:
reuse them or write your own.

You touch `perf.rs`, the sampling layer every collector's profile uses: a full
`make check` before each commit.

Handoff: `docs/unwind.md`.

---

## Stream D -- it installs

> **Mission.** Nobody but the developer has ever run `omn`. Make the first ten
> minutes of someone else's possible: download or build, grant the two
> capabilities, and see a real result, on a stock distribution.

1. **A release**: `scripts/release.sh` builds the release binaries (`omn`,
   `omn-mcp`, the preload shim) and the manual page into one archive with its
   checksum; `make install PREFIX=...` installs them, the shim where the
   binary looks for it, and says how to grant the capabilities rather than
   granting them itself; `.github/workflows/release.yml` builds the archive on
   a tag. What the binary finds at run time (the shim beside it, the catalog
   compiled in) is checked from the installed tree, not the build tree.
2. **`docs/install.md`**: what a stock Ubuntu, Fedora and Arch need -- the
   kernel floor, the two capabilities and why, what `perf_event_paranoid` does
   to what is available, and the one-line checks (`omn list`) that tell a
   user what their machine serves. Only what is measured or read from the
   code; a claim about a distribution this machine is not is marked as read
   from its documentation.
3. **`README.md` and `docs/DEMO.md` for a stranger**: the README's first screen
   says what the tool is, how to install it and the one command to try; the
   long material moves below or out. `docs/DEMO.md`'s outputs are regenerated
   after the other streams merge, by the coordinator, so leave them.

**Ground truth**: `make install` into a temporary prefix, the build tree moved
out of the way, and the installed `omn` run from there: `omn list`, a launched
`mem.rss`, and `mem.heap.alloc` (which needs the shim) all serve. The archive's
checksum verifies and its contents are exactly what `release.sh` lists.

You own the `Makefile`, which every stream runs: keep `make`, `make check` and
the capability step exactly as they behave now, and add targets beside them.

Handoff: `docs/install.md`, a section "For the coordinator" at its end.

---

## Rules

The three that have held for thirteen waves:

1. **The unit of ownership is a file set nobody else writes.** The table above.
2. **The handoff is a file only that stream can write**: P's is
   `docs/portability.md`, R's `docs/fixes/results.md`, K's
   `docs/families/kernel.md`, J's `docs/families/join.md` and `docs/mcp.md`,
   D's `docs/install.md`, U's `docs/unwind.md`. End it with **the index rows for every session you
   added**, **what is for `docs/DECISIONS.md`**, **anything you filed for another
   stream**, and **the matrix cells your change should move**.
3. **A task is landed when its scenarios are**: ground truth, the capability
   sweep, the loss path, the refusal.

**Build with `make`, not `cargo build`** -- the preload shim and the
capabilities both fail silently otherwise. One branch per stream, made by
`scripts/spawn.sh`. Commit there; the coordinator merges.
