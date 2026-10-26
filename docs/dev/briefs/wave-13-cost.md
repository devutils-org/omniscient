# Brief — what a run costs the target, the targets nothing serves, and the engine's words

Five work streams. The thirteenth fan-out, and the first with no milestone
left to land: `docs/dev/STATE.md`'s "What remains" is now finding 47, the per-ring
losses the eleventh wave's merge left, P11, and the requests twelve waves have
filed against files that changed hands. This wave is those, grouped by the
files they land in.

Read `CLAUDE.md` first -- **including "How much to test, and when", which this
wave is held to** -- then `docs/dev/STATE.md`, then your section here, then
`docs/contributing.md`. The twelve earlier briefs are in `docs/dev/briefs/`.

**This is the only brief.** What comes after this wave is written in
`docs/dev/STATE.md` under "What remains", and nowhere else.

---

## Before anything else: how you test

The full suite is about 1800 tests and takes 5 to 10 minutes, because some 350
scenarios measure the real machine. The rule:

- **While you work**: run only your own module and your own test files --
  `cargo test --lib <module>` and `cargo test --test <your file>`. Never
  `make check` while iterating.
- **Before each commit**: one `make check`. If it is red, run
  `cargo test --workspace --no-fail-fast` once to see every failure together.
- **A red test outside your paths**: look it up in `docs/flaky.md` (the audited
  tally) and the "for the flaky tally" notes of `docs/open-findings.md`. If it
  is there, rerun it alone once and move on. Do not rerun the suite for it, and
  do not try to fix it -- it is not yours.
- **`make`** once per build, before eBPF tests, because a relink drops the
  capabilities. And **run `cargo fmt` before `make`, never after it**: a
  format that changes a source makes the next test build relink `omn`
  without its capabilities, and every eBPF test then fails as unprivileged.

Five streams and a coordinator share this machine. Assert the separation, not
a threshold calibrated on a quiet machine.

---

## What changed before this wave

**The twelfth wave landed all five of its streams** -- the flight recorder,
`omn-mcp`, finding 48, what the resolver promises, and what a probe asks --
and `docs/dev/briefs/wave-12-recorder.md` keeps its brief. What they filed is in
`docs/open-findings.md` under "the twelfth wave", one heading per stream.

**`docs/open-requests.md` is a snapshot taken before the twelfth wave**, and
that wave closed a good part of it. Every item below that cites a line of it
(`open-requests 2755`) is to be **checked against the code first**: one that
has landed is a line in your handoff, not work.

**`main` is pushed, and the CI workflow runs on every push from now on.** Its
first run is the unprivileged cell `docs/portability.md` describes. Its log
reaches this wave through the coordinator; stream P owns `.github/**` and
answers what it shows.

**The portability matrix has no cells on disk** (`target/matrix/cells/` was
emptied). The judge is vacuous until they are swept again, which stream P does
for the cells its fixes touch, and the coordinator for the rest after the wave.

**The bench does not run during a wave.** Its variance gate refuses a loaded
machine; `docs/bench.md` has its one command, for after this wave.

---

## The streams

| | Stream | Delivers | Owns |
|---|---|---|---|
| **R** | what a run costs the target | finding 47: the cost and the budget read what an in-kernel or in-process collector spends inside the target; losses per ring; the runner's and the emit layer's filed requests | `src/collect/mod.rs`, `src/collect/launch.rs`, `src/collect/task.rs`, `src/main.rs`, `src/session.rs`, `src/request.rs`, `src/cli.rs`, `src/emit.rs`, `src/encode.rs`, `src/render.rs`, `src/triage/**`, `src/bpf/syscall.rs`, `shim/**`, `src/collect/mem/heap/**`, the `cost:` key of every backend line in `catalog/*.yaml`, `scripts/bench.sh`, `tests/budget.rs`, `tests/runner.rs`, `tests/triage.rs`, `tests/recorder.rs`, `tests/encoders.rs`, `tests/aggregation.rs`, `tests/bench_*.rs`, `examples/record-*.yml`, `man/omn.1`, `docs/fixes/budget.md` (new) |
| **P** | the targets perf can serve | P11; the `cgroup` and `container` targets the counting sources declare and nothing serves; the resolver's and the loader's filed requests; the CI's first run | `src/perf.rs`, `src/resolve.rs`, `src/catalog.rs` (except `MatchKind`, J's), `src/validate.rs`, `src/filter.rs`, `src/capabilities.rs`, `src/bpf/usdt.rs`, `src/collect/cpu/**`, `src/collect/mem/page_faults.rs`, `src/collect/power/**`, `src/collect/gpu/**`, `src/collect/runtime/**`, the `cpu.*`, `power.*`, `gpu.*`, `runtime.*` and `mem.faults` entries of `catalog/*.yaml`, `tests/scenario_{cpu,power,gpu,runtime}.rs`, `tests/family_{cpu,power,gpu,runtime}.rs`, `tests/portability.rs`, `tests/catalog_model.rs`, `scripts/matrix.sh`, `scripts/vm/**`, `.github/**`, `examples/{cpu,power,gpu,runtime}-*.yml`, `docs/portability.md` |
| **S** | the scheduler and the processes, per cgroup | the `sched` and `proc` programs serve the cgroup their entries declare; `sched.switches` counts; what the eBPF layer's own requests left | `src/bpf/insn.rs`, `src/bpf/btf.rs`, `src/bpf/attach.rs`, `src/bpf/mod.rs`, `src/bpf/map.rs`, `src/bpf/ring.rs`, `src/bpf/fentry.rs`, `src/bpf/wakeup.rs`, `src/bpf/sched_*.rs`, `src/bpf/runqlat.rs`, `src/bpf/offcpu.rs`, `src/bpf/migrate.rs`, `src/bpf/proc_*.rs`, `src/collect/sched/**`, `src/collect/proc/**`, the `sched.*` and `proc.*` entries of `catalog/*.yaml`, `tests/scenario_{sched,proc}.rs`, `tests/family_{sched,proc}.rs`, `tests/raw_tracepoints.rs`, `examples/{sched,proc}-*.yml` (except `sched-wakeup.yml`, J's), `docs/families/kernel.md` |
| **N** | the older kernels serve what they can | the `io`, `net` and `mem` programs read the shapes 5.15 and 6.6 write; the requests filed against those collectors | `src/bpf/io.rs`, `src/bpf/io_*.rs`, `src/bpf/net_*.rs`, `src/bpf/mem_*.rs`, `src/bpf/request.rs`, `src/bpf/request_net.rs`, `src/collect/io/**`, `src/collect/net/**`, `src/collect/mem/**` except `page_faults.rs` and `heap/**`, `src/collect/psi.rs` (new), the `io.*`, `net.*` and `mem.*` entries of `catalog/*.yaml` except `mem.faults`, `tests/scenario_{io,net,mem}.rs`, `tests/family_{io,net,mem}.rs`, `examples/{io,net,mem}-*.yml`, `docs/families/devices.md` (new) |
| **J** | the engine says what it knows | the evidence ladder in words, the match kinds the crossings asked for, stacks a reader can read, and `omn-mcp` on the library's answers | `src/join.rs`, `src/event.rs`, `src/stack.rs`, `src/clock.rs`, the `MatchKind` enum of `src/catalog.rs` and its parsing, `mcp/**`, `tests/join_engine.rs`, `tests/cross_*.rs`, `tests/stack_capture.rs`, `tests/vertical.rs`, `examples/cross-*.yml`, `examples/join-*.yml`, `examples/vertical-*.yml`, `examples/sched-wakeup.yml`, `docs/families/join.md` |

**A catalog entry belongs to the stream that owns its collector**, whole,
except the `cost:` key of each backend line, which is R's this wave (finding
47's bands). `docs/CATALOG.md` is the coordinator's: a notation you add is a line
for it in your handoff.

**Nobody owns** `src/error.rs`, `src/lib.rs`, the tests not listed, and
`tests/cli.rs`. A stream that needs one writes the request under `## Needs
from another stream` in `docs/open-findings.md`, **under a heading naming the
stream and the thirteenth wave**, with the exact signature, and carries on.
That file is append-only per stream.

**Three splits to respect.**

- `src/bpf/syscall.rs` is R's, because finding 47 needs the kernel's own
  account of a program's run time (`BPF_OBJ_GET_INFO_BY_FD`). The fentry
  commands `open-requests 5536` asks to move there are not this wave's.
- `src/resolve.rs` is P's and `src/render.rs` R's. Stream W's request (R
  prints who applied a predicate) leaves a `no;` prefix in `impl Display for
  Placed` to delete once R's line lands: **P leaves it**, and the coordinator
  deletes it at the merge.
- `MatchKind` is J's inside P's `src/catalog.rs`. A new variant is J's; the
  loader's checks over it are P's, and J files what it needs of them.

**The coordinator owns** `examples/README.md`, `docs/GUIDE.md`, `docs/dev/STATE.md`,
`README.md`, `docs/DECISIONS.md`, `docs/ROADMAP.md`, `docs/CATALOG.md`, `docs/bpf-layer.md`,
`docs/families/cpu.md`, the `Makefile` and `scripts/spawn.sh`. The
session-index test will be red on your branch for the sessions you add.
**That is expected** -- list the rows in your handoff. A decision worth
recording goes in your handoff under "For `docs/DECISIONS.md`"; the coordinator
writes it.

---

## Stream R -- what a run costs the target

> **Mission.** Finding 47: `proc.syscall` in `count` costs 0.42 % of the
> target's cpu in `cost.json` and **+296 %** in the target's own timed section,
> because the program runs on every syscall in the target's context. The cost
> this tool reports and the budget it holds are the tool's own cpu, and for an
> in-kernel collector that is not where the cost is.

1. **The cost a collector spends inside the target is measured, or said to be
   unknown -- never zero.** `collect::Overhead` gains what was spent inside the
   target (`open-requests 1845` names it `in_target_cpu_ns`), `cost.json`
   carries it beside the tool's own cpu, and `--budget` weighs both. The
   mechanism is yours, and it is the decision to write down. What is there to
   read: the kernel's per-program `run_time_ns` and `run_cnt` in
   `bpf_prog_info`, counted only while statistics are enabled
   (`BPF_ENABLE_STATS`, or `kernel.bpf_stats_enabled`) -- find out what
   enabling them needs, and what a run where it cannot be enabled says; the
   preload shim's own time, which it can publish in its ring's header; and a
   launched target that times itself, which is the bench's ground truth rather
   than a mechanism. A probe answers available, missing or unknown, and so does
   this.
2. **The bands follow the measurement.** The catalog's bands for per-event
   raw-tracepoint backends were set on the tool's cpu; reconsider them against
   `docs/bench.md`'s dilation column (`sched.futex` `bpf:raw_tp` is still
   `cost: med`), and write the rule for choosing a band in your handoff.
3. **Losses per ring.** A profile keyed by `task` cannot say which task lost
   which samples, though each ring belongs to one (`docs/fixes/silent.md`, and
   `docs/dev/STATE.md`'s note on `cross_compute`). `Collection` carries losses per ring,
   and a series says its own `Shortfall`. What this changes for
   `tests/cross_compute.rs` (J's) is a line in your handoff.
4. **What the shim buffers is bounded.** The lock storm of finding 47 held
   3.9 GiB of what it had read: a bound on what the reader holds before it drops
   and counts.
5. **The filed requests, in this order**, each checked first: a counted window
   ends at its duration, not at the next tick (`open-requests 2947`, `6181`);
   `omn list --format json` carries the sweep, `Listed` as stream M wrote it,
   with `resolve::probe_all` now public; `main.rs` calls
   `Cost::worst_case_percent` instead of its own copy; the filter line of the
   text rendering says who applied the predicate, as stream W wrote it;
   `topk:N@<attr>`, a ranking over an attribute (`2755`, `3883`, `2600`), and
   `count` that can rank (`2467`); `Results.overhead` and `served_by`
   (`2316`); a note that is not a degradation (`3892`); a rate over the
   counter's own enabled time (`168`); `Results.period_ns` for pprof
   (`2127`); `HistScale` serialised and `hist:linear:0` refused at parse
   (`314`, `323`); a multi-pid request refused before the launch by a
   collector that takes one pid (`250`). What you do not reach is a line in
   your handoff with where it stands.
6. **The tally's rows in your files**: `docs/flaky.md` #11 to #15 and #27,
   each with the one-line fix the audit wrote.

**Ground truth**: for 1, the bench's own workload -- `proc.syscall` in
`count` on a target that times its syscall loop -- where the in-target cost
the run reports stands against the dilation the target measured, within the
spread of three runs, and a run on which the statistics cannot be enabled says
*unknown*; for 3, two tasks in one profile, one of them made to overflow its
ring, and the shortfall lands on that task's series only; for 4, the storm of
finding 47, bounded, with the drops counted against what the workload says it
did.

You touch the runner and the emit layer, shared layers: a full `make check`
before each commit.

Handoff: `docs/fixes/budget.md`.

---

## Stream P -- the targets perf can serve

> **Mission.** The counting sources, `cpu.profile` and `mem.faults` declare
> `cgroup: native` and `container: native`, and their collectors refuse both:
> P8's shape one target over, which stream W found and V left for this wave.
> And the one matrix class nobody has read: P11.

1. **P11** (`docs/portability.md`): on 5.15 under `caps`, `cpu.profile
   --system` is refused at the ring (*"mmap of 8 ring pages: Operation not
   permitted"*). Confirm the cause -- the likely reading, unconfirmed, is that
   5.15 charges per-cpu rings against `perf_event_mlock_kb` and only
   `CAP_IPC_LOCK` exceeds it -- then either a probe that sees it or a refusal
   that names it, before the start. Ground truth is the 5.15 cell.
2. **The `cgroup` and `container` targets**, over `perf::Scope::CgroupOnCpu`:
   write the collectors, or take the keys out of the entries in the same
   commit. Decide per source and say why.
3. **The CI's first run.** The coordinator passes its log on; every step that
   is red, and every skip the gate refuses, is yours to answer in
   `.github/**`, or to file where it is not.
4. **The loader and the resolver**, each checked first: `check_catalog` runs in
   the loader (`open-requests 2493`), and a key the backend cannot attach is
   refused (`2539`); `check_stack` refuses `dwarf` and `lbr` where the
   collector cannot (`815`), and `stack` over a non-additive unit (`2127`); a
   `pmu:` glob is refused by the loader and a qualified `pmu:cpu/x` that is
   absent answers missing, not unknown (`1598`); one reader of `cpu/online`
   (`1649`); a `writable:` requirement (`1422`); `SampledEvent::throttled()`
   (`885`); `Unit::Watts`, and `gpu.power` in watts (`957`); a per-tick read of
   a counting PMU event (`5099`).
5. **The tally's rows in your files**: `docs/flaky.md` #8, #9 and #24, and
   `open-requests 5272` (the known-instruction scenario at +0.69 %).
6. **The matrix**: sweep again the cells your fixes touch, on the four kernels,
   and leave `KNOWN_DEFECTS` holding only what still fails.

**Ground truth**: for 1, the 5.15 cell; for 2, a cgroup holding a workload
that retires a known number of instructions (or faults a known number of
pages) beside the same workload outside it, and the run counts one and not
the other; for 4, a catalog copy carrying each refused shape, refused by the
loader with the reason.

You touch the resolver, the catalog loader and the probes, shared layers: a
full `make check` before each commit.

Handoff: `docs/portability.md`, a section "The thirteenth wave".

---

## Stream S -- the scheduler and the processes, per cgroup

> **Mission.** Six `sched` entries and three `proc` entries declare `cgroup:
> bpf_only`, and every one of their collectors refuses a cgroup
> (`open-requests 4328`, `4168`: *"needs a helper the eBPF layer does not have
> yet"*). The layer is yours this wave, so the helper is too.

1. **A program narrowed to a cgroup**: the helper and the comparison in
   `src/bpf/insn.rs` (`bpf_get_current_cgroup_id` against the cgroup's id, or
   `bpf_current_task_under_cgroup` over a `CGROUP_ARRAY` -- yours to choose and
   say why), the cgroup path resolved to its id, and the `sched` and `proc`
   collectors serving `--cgroup`. A source you leave unserved loses the key in
   the same commit.
2. **`sched.switches -m count`**: the collector counts, on
   `crate::collect::cpu::counters::Prepare::new(&counters::SCHED_SWITCHES,
   Mode::Count)` as stream W described it (P's file: use it, do not change
   it), and `scenario_sched`'s `a_declared_mode_the_build_refuses_...` moves to
   what is still refused. Its `cgroup` and `container` keys as in item 1.
3. **What `sched` and `proc` still carry wrong**, each checked first: `comm` is
   the name at row creation (`open-requests 1392`); `sched.offcpu --emit hist`
   needs a bucket in its key (`624`) and `TOTAL_OF_ATTR` (`2641`); the soft
   half of `sched.irq` (`4304`); a thread target that compares the thread
   (`4927`); `proc.syscall`'s program counts what its comparison skipped
   (stream W's request, twelfth wave).
4. **The catalog's words**: `sched.wakeup`'s interrupt caveat as stream D wrote
   it; the observer effect on `sched.lock`, `sched.offcpu` and
   `sched.runqlat` (finding 47); `sched_process_exit(task_struct *)` on 6.6
   as a second shape (stream V's list).
5. **The tally's rows in your files**: `docs/flaky.md` #1, #3, #5, #6 and #7.

**Ground truth**: for 1, a cgroup holding a workload that blocks, switches or
forks a known number of times, beside the same workload outside it -- the run
sees the first and none of the second; for 2, N threads each switching a known
number of times.

You touch the eBPF layer (`insn.rs`, `btf.rs`, `attach.rs`, `map.rs`), a
shared layer: a full `make check` before each commit.

Handoff: `docs/families/kernel.md`, a section "The thirteenth wave".

---

## Stream N -- the older kernels serve what they can

> **Mission.** The twelfth wave made the probes tell the truth, and on 5.15 and
> 6.6 the truth is that several `io`, `net` and `mem` sources are missing
> because their program reads a shape one slot away from what the kernel
> writes. Each of those is a program that could read both.

1. **The widenings stream V measured**, each a second shape in the program and
   in its `raw_tracepoint:` requirement, in the same commit: `sock.sk_ino`
   through `sk_socket->file->f_inode` on 6.6 and older (`net.drops`,
   `net.tcp.lifetime`, `net.tcp.retransmit`); 6.6's
   `mm_vmscan_direct_reclaim_begin(int, gfp_t)`; 5.15's `kmalloc` (five
   arguments), `block_rq_complete(request *, int, unsigned int)` and
   `mm_compaction_begin` (four integers).
2. **What the collectors still say wrong**, each checked first: `net.drops`
   names its reason (`open-requests 3796`) and the remote endpoint is read
   (`3760`); a `mem.page_alloc` or `mem.kmalloc` free is an event or refused as
   a filter, not a note (`3972`); `net.iface` splits by counter kind (`2585`)
   and `daddr` stops carrying the port (`2745`); `mem.swap` gets a counted
   window (`2619`); the PSI reader exists once, in `src/collect/psi.rs`
   (`1503`); 2.6 MiB of flusher progress beside 90 MiB of kworker writes
   (`5132`), investigated.
3. **The catalog's words**: the 3 MiB of shim ring inside a `mem.rss` read
   (finding 47); the multi-size THP caveat the measurement contradicts
   (`4699`); attach dominating a short `fentry` run, on `io.fsync` and
   `net.tcp.connect` (finding 47).
4. **The tally's rows in your files**: `docs/flaky.md` #16 to #21 and #23.

**Ground truth**: for 1, the matrix cell each shape failed -- `scripts/matrix.sh
v5.15.221 --configs caps` and `v6.6.157` (P owns the script; running it is
fine), mind the disk -- where the source goes from missing to attaching and
its scenario's workload is seen; and the host cell keeps attaching what it
attached. For 2, the workload each request names.

Handoff: `docs/families/devices.md`.

---

## Stream J -- the engine says what it knows

> **Mission.** `src/join.rs` has had no owner for two waves, and four requests
> waited on it, one of them from `omn-mcp`, which keeps its own copy of what an
> evidence level means until the engine says it.

1. **The evidence ladder in words**: `Evidence::establishes` and
   `does_not_establish`, as stream M wrote them; `omn-mcp` calls them and its
   copy goes. The text renderer printing them is R's: file it.
2. **A waker of 0 is not always the idle task**: the chain's first hop is
   named `interrupt (hardirq)`, `(softirq)` or `(nmi)` when the event's
   `waker_context` says so (stream D, twelfth wave).
3. **The match kinds the crossings asked for**, each checked first: `Woke`,
   a close paired with the next wakeup of its task (`open-requests 4968`); a
   window-overlap match that is `TEMPORAL` by construction (`5032`); a
   decomposition part defined by a join, and a traced span as the whole
   (`4988`); a block key the plug's cached clock cannot collapse (`3713`).
4. **Stacks a reader can read**: symbols demangled (`open-requests 710`), Rust
   and C++. A new crate is a decision to justify in your handoff.
5. **A NUMA node has a key** (`open-requests 1469`).
6. **The tally's rows in your files**: `docs/flaky.md` #10, #25 and #26, and
   the comments that say one run observes one target (`5256`) in yours.
7. **`omn-mcp` launches the sessions the operator wrote** (added mid-wave,
   decided by the user): `--allow-launch DIR`, meaningful only with
   `--allow-run`. A launch target is accepted only from a session file in DIR,
   exactly as the file states it: program, arguments, environment and working
   directory. The model chooses which approved session runs and when, never
   what runs. Duration and budget stay bounded as they are. The approved
   sessions are discoverable, and whether a repeated, spread-reporting run is
   its own tool is yours to decide and write down. It goes after items 1 and 2.

**Ground truth**: for 1, every evidence level a real session's joins produce,
read back through `omn-mcp` with its sentence; for 2, finding 48's
reproduction, where the interrupts' wakeups are named as interrupts; for 3,
the workload of the crossing that asked, and the attribution the new match
makes against what that workload says it did; for 4, a Rust and a C++ fixture
whose stacks come back with their source names.

You touch the join engine, a shared layer: a full `make check` before each
commit.

Handoff: `docs/families/join.md`, a section "The thirteenth wave".

---

## Rules

The three that have held for twelve waves:

1. **The unit of ownership is a file set nobody else writes.** The table above.
2. **The handoff is a file only that stream can write**: R's is
   `docs/fixes/budget.md`, P's `docs/portability.md`, S's
   `docs/families/kernel.md`, N's `docs/families/devices.md`, J's
   `docs/families/join.md`. End it with **the index rows for every session you
   added**, **what is for `docs/DECISIONS.md`**, and **anything you filed for
   another stream**.
3. **A task is landed when its scenarios are**: ground truth, the capability
   sweep, the loss path, the refusal.

**Build with `make`, not `cargo build`** -- the preload shim and the
capabilities both fail silently otherwise. One branch per stream, made by
`scripts/spawn.sh`. Commit there; the coordinator merges.
