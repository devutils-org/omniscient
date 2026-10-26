# Brief — what is left before the comparison

The twentieth wave: everything `docs/dev/STATE.md` lists under "After the nineteenth
wave", items 2 to 11, so that the wave after it can do the last item -- the
same numbers as the reference tools -- on a tool with nothing else pending.
Item 1 (the runs alone) is the coordinator's and ran before this wave; item 5
(the CI read back) is the coordinator's and runs after it.

It runs as **fourteen streams in two phases**: eleven at once (the nine
family and layer streams, view and workloads), then record, process and docs,
which need them. Each stream is merged as soon as it is done, one merge at a
time, and reviewed while it is merged: a review's fixes come as a second
merge. Speed is chosen over certainty here -- what slips through is caught by
the full suite at the end and fixed then.

Read `CLAUDE.md` first -- **including "How much to test, and when" and its
section "In a wave of parallel streams", which this wave is the first held
to** -- then `docs/dev/STATE.md` "After the nineteenth wave" (your item, by grep and
line range), then your section here. Each section names the `docs/dev/STATE.md` items
it carries; the bullets there are the work, and the handoffs they cite
(`docs/fixes/w18-*.md`, `docs/fixes/w19-*.md`) are the detail.

**This is the only brief.** What comes after this wave is written in
`docs/dev/STATE.md` under "What remains", and nowhere else.

---

## Decided before this wave

- **All nine views are kept** (the user's verdict, 2026-10-04, after using
  them). The view stream does what each asked for, and the record stream
  fills the gallery's `Keep?` column with "yes (2026-10-04)".
- **The views are linked, and a process has one page of its own** (the user,
  2026-10-04). From any view, what is selected opens the view that explains
  it: a process in `system` opens its `ledger`; a ledger line opens `flame`
  (cpu) or `timebar` (blocked time); a slow request in `request` opens `lanes`
  and `wake` on its moment and its threads; and back. And a new view,
  **`process`**, is the first screen of `omn top -- <program>` (and of
  `--pid`): every view's essence for that one process on one screen, with the
  minimum of detail -- a number and a colour per tile, read at a glance --
  each tile opening the view that has the detail, by key or by click. The colour says what the
  tool found (the triage's first suspect, the dominant resource, a figure's
  trust), never an arbitrary threshold. `system` stays the first screen
  without a target.
- **The view is clickable** (the user, 2026-10-04). The selection the links
  carry is a set of regions the view drew -- a tile, a frame, a lane, a
  ledger line, a request, a heatmap cell -- each with its rectangle, so the
  mouse costs little once the links exist: a click selects, a click on the
  selection (or a double click) goes in, the wheel scrolls or zooms where the
  view zooms, a right click or the back button comes back. Keys stay complete
  on their own: nothing is reachable by mouse only. Mouse capture is on by
  default and one key turns it off, because capture takes the terminal's own
  text selection away.
- **Every view has a workload written for it** (the user, 2026-10-04): a
  program whose load is known in advance and exercises everything that view
  can draw -- for `flame`, functions of known weight on and off cpu across
  threads; for `request`, requests each slowed by a known cause; for `wake`,
  a known chain of hand-offs; for `flows`, known bytes per file and per
  socket; for `heatmap`, latency regimes in known phases; for `lanes`, threads
  and GPU kernels in a known schedule; for `timebar`, a known split of wall
  clock by state; for `ledger` and `system`, a known cost on every resource;
  for `process`, all of it at once. Each declares its truth in a manifest
  beside it. Two uses: **ground truth** -- the view's numbers are checked
  against the manifest, within each figure's stated trust, the way every
  source is held -- and **rich, reproducible fixtures**: the recordings the
  snapshots are drawn on come from these programs, so a re-recording draws
  the same screens, the numbers moving only within their measurement. The
  realistic recordings from `cases/` stay beside them.
- **A field, not a sentence.** Where a collector says something in a caveat or
  a note that a program would want to read -- `net.request`'s never-ended,
  unborn and replaced counts, the start of its longest sleep -- it becomes a
  field of the result, and the sentence stays for the human.
- **The comparison against the reference tools is the next wave**, not this
  one. Nothing here runs `perf`, `bpftrace`, `nsys` or `ncu` except as an
  independent reader a test already uses.
- **The open decisions stay open unless a stream's measurement settles one**:
  the budget's first check (stream budget proposes, with numbers), a cost per
  regime in the catalog's schema (stream gpu proposes). A proposal is a
  decision line in the handoff, with what it costs to leave it open.

---

## How you test, and how you read

- **Test what you produce, nothing else**: the tests of your own files, then
  `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo fmt --check`. **No `make check`, no `cargo test --workspace`** --
  unless your section says you own a shared layer (`src/perf.rs`, `emit.rs`,
  `encode.rs`, `join.rs`, `btf.rs`, `insn.rs`, `map.rs`, the catalog loader),
  and then one `make check` before your last commit.
- A test outside your files that fails in your run is not yours: rerun it
  alone once, and if it fails again, file it in your handoff.
- **`cargo fmt` before `make`, never after.** `make` once per build, before
  eBPF tests.
- **Read by range**: `grep -n`, then the lines you need.
- **Nine agents share this machine.** Assert separations, not thresholds tuned
  on a quiet one. Do not start `scripts/matrix.sh` or `scripts/bench.sh`.
- **Privileged commands**: `sudo -n` only, and only what `sudo -n -l` lists.
  Do not change a sysctl, load a module or touch the GPU's clocks. List the
  run you want in your handoff with its exact command.
- **Disk**: your worktree builds without incremental compilation (its
  untracked `.cargo/config.toml`; never commit it). Clean `/tmp` of what your
  tests leave.

## Where you work, and how you hand off

Each stream has its own worktree, `/home/moha/repos/omniscient-w20-<stream>`,
on `feat/w20-<stream>`. The handoff is `docs/fixes/w20-<stream>.md`: what
landed, shown versus reasoned, what you could not do and why, the privileged
runs you want, your decision lines, and the requests to other files under
"Needs from another stream". A request to a file you do not own is filed
there, not made.

---

## The streams

| Stream | Owns | Items | Phase |
|---|---|---|---|
| **gpu** | `src/collect/gpu/**`, `cupti/**`, `catalog/gpu.yaml`, `src/validate.rs`'s `target_rejection_reason`, the clock-fact lines of `src/render.rs`, `docs/scenarios/gpu.md`, `docs/families/gpu.md`, `tests/{gpu_kernel,gpu_counters,family_gpu,scenario_gpu}.rs` | 3 | A |
| **rings** | `src/perf.rs` (shared), `src/collect/cpu/profile.rs`, `src/unwind.rs`, `catalog/unwind.yaml`, `tests/{runner,unwind,stack_capture}.rs` | 4, 11 (unwind) | A |
| **triage** | `src/triage/**`, `tests/triage.rs` | 6 | A |
| **budget** | `src/main.rs`, `src/collect/{mod,launch,task}.rs`, `src/stack.rs`, `mcp/**`, `tests/budget.rs`, `docs/mcp.md` | 7, 10 (mcp) | A |
| **sched** | `src/collect/sched/**`, `src/bpf/{sched_*,offcpu,runqlat,wakeup,switch_count,migrate}.rs`, `catalog/sched.yaml`, `tests/{scenario,family}_sched.rs`, `examples/sched-*.yml` | 8 (sched), 11 (lock, futex) | A |
| **net** | `src/collect/net/**`, `src/bpf/{net_*,request_net}.rs`, `catalog/net.yaml`, `tests/{scenario,family}_net.rs` | 8 (net) | A |
| **cpu** | `src/collect/cpu/**` except `profile.rs`, `src/collect/power/**`, `src/bpf/` for a new `cpu.oncpu_time` program, `catalog/{cpu,power}.yaml` except `cpu.profile`, `tests/{scenario,family}_{cpu,power}.rs` | 8 (cpu, power), 2 (`raw` on `cpu.util`, `cpu.oncpu_time`) | A |
| **io** | `src/collect/io/**`, `src/bpf/{io,io_*,request}.rs`, `catalog/io.yaml`, `tests/{scenario,family}_io.rs`, `docs/scenarios/io.md` | 8 (io), 11 (io), 2 (`raw` on `io.bytes`, `io.syscall`) | A |
| **shared** | `src/join.rs` (shared), `src/catalog.rs` (shared), `src/collect/mem/footprint.rs`, `src/cli.rs`, `tests/{cli,cross_machine,join_engine}.rs`, `man/omn.1` | 9 | A |
| **view** | `view/**` except `view/workloads/**`, `src/emit.rs` and `src/encode.rs` (shared), `tests/view_{model,live,timebar}.rs`, `docs/view/*.md`, `docs/GUIDE.md`'s `omn top` session | 2 | A |
| **workloads** | `view/workloads/**` (new: one program and one truth manifest per view, and `process`'s), `tests/view_workloads.rs` (new) | 2 | A, beside the others |
| **record** | `tests/view_fixtures/**`, `docs/view/gallery/**` except `process`'s, `tests/view_snapshots.rs`, `tests/view_truth.rs` (new) | 2 | B, once all of A is merged |
| **process** | `view/src/views/process.rs`, its snapshots, `docs/view/process.md` | -- | B, once view and workloads are merged |
| **docs** | `docs/CATALOG.md`, `docs/unwind.md`, `examples/` except `sched-*.yml`, `docs/GUIDE.md` except `omn top`, `tests/bench_harness.rs`, `tests/cases.rs`, and the flaky tally's entries in files nobody else owns | 10, 11 (flaky) | B, once all of A is merged |

`docs/DECISIONS.md`, `docs/dev/STATE.md`, `docs/ROADMAP.md`, `docs/open-findings.md` and the
indexes are the coordinator's. A catalog entry's lines belong to the stream
that owns its collector.

### gpu -- what the GPU's streams left (item 3)

**First, a defect the bench found after the wave** (`docs/open-findings.md`,
"The nineteenth wave, coordinator, after the merge"): `gpu.profile` crashes a
native CUDA program at exit -- `free(): double free detected in tcache 2`,
SIGABRT, in an exit handler of `libomn_cupti.so` while a CUPTI thread still
runs, 2 of 2 -- and `omn` exits 0 reporting only a lost read. Fix it, hold it
with a native (not Python) workload in the tests, and make a target killed by
the injection library a failure the run says, not a lost read. Then
`gpu.temp` over its `low` band on the compute-bound workload (the band or the
collector), and the GPU bench's two hours (`gpu.counters`' replay of the
launch-bound workload took 108 of 117 minutes: a shorter launch-bound run for
the replayed backends).

The counters-beside-kernel refusal made the request's, before launch, exit 2;
no unwinding warning for `gpu.profile`; the run's environment carrying the
clock states when a `gpu` source is asked, and `render.rs` printing the fact;
`kernel.rs`'s read path profiled (1.3 us a record) and a lighter activity kind
as a fallback if one exists; `target_rejection_reason` reviewed against a
`targets: {pid: "no: <reason>"}` form, the better one kept; the stale
`vendor:ncu` lines; `gpu.mem` and `gpu.engine_busy` accepting `raw` (item 2,
8). Propose the cost-per-regime decision with numbers.

### rings -- `make check` green (item 4, and unwind from 11)

`cpu.profile`'s `losses_by_ring` carries each ring's throttles, with the
per-ring counts from `src/perf.rs`, so
`runner::a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed`
passes on a quiet machine and under load. `unwind`'s stripped-build second
shape. You own a shared layer: one `make check` before your last commit.

### triage -- what the re-asked cases found (item 6)

A job is a chain of processes (`make-sqlite`, `gcc-shell`); a thread that
waits beside the main thread (`http-server-stdlib`'s slow client); the tool's
own eBPF frames never the named suspect; one spelling of `cpu-queue` and
`other-block`; `pid` on profile samples and the late discovery; the
lossy-profile verdict under load decided. Hold each to a workload whose
answer is known, in `tests/triage.rs`.

### budget -- the budget and the run's account (item 7, and `docs/mcp.md` from 10)

Every bullet of item 7; propose the first-check decision with numbers (what a
one-time cost weighs against a few milliseconds of target). `docs/mcp.md`
follows what `mcp/` does now.

### sched -- the sched collectors the views found short (item 8, 11)

`sched.wakeup`'s waker and wakee names, the irq number and `served_in_tid` on
interrupt wakeups; `sched.offcpu`'s stamping said truthfully; `sched.lock` and
`sched.futex` following descendants; `examples/sched-round-trips.yml`'s and
`sched-half-blocked.yml`'s headers.

### net -- the network sources the views found short (item 8)

`net.request`'s bytes per request, the start of its longest sleep, its
never-ended, unborn and replaced counts as fields; `net.socket` carrying its
route's interface; `net.bytes` split by direction and accepting `raw`;
`net_qdisc`'s namespace offset through `member_offset`.

### cpu -- exact on-cpu time, and `raw` (item 8, 2)

A `bpf:raw_tp` collector for `cpu.oncpu_time`, exact per thread, held to the
kernel's own `schedstat`; `raw` on `cpu.util`, `cpu.oncpu_time` and
`power.energy`.

### io -- the storage sources the views found short (item 8, 11, 2)

A filesystem's superblock device mapped to the block devices under it;
`io.bytes` split by direction; `raw` on `io.bytes` and `io.syscall`; a known
number of merges driven and counted; `io.inflight`'s cause on 6.17; one
program for both `io.latency` histograms if it falls out.

### shared -- the shared layers and the files nobody owned (item 9)

Every bullet of item 9: `assemble` and `sources_read`; `NamedBy` and stack
rows keyed by the request's keys; `cheaper_equivalents` into the catalog
loader; `footprint.rs`'s ESRCH re-read; `cross_machine`'s per-call cgroup;
the executable of each task a result keys; `omn join` over a stored run, with
`tier:` in `man/omn.1`; the `cli` tests' `/tmp` leak. You own shared layers:
one `make check` before your last commit.

### view -- what the views asked for (item 2)

All nine views, kept (the user's verdict). In the frame: the
`View::takes_text` hook, `--against` and a focus in the snapshot harness,
`units::approx`, `as_live_until` saying a source kept without `raw` arrives
empty, the flame diff held to `src/triage/diff.rs`, the README's `why`
column checked, the frame snapshots drawn on a steadier view, a scale-aware
dimming rule. In `results.json` (`emit.rs`, `encode.rs`): each timeline's
`first_ts_ns`, each result's value attribute, how a drained source stamps its
rows, each drain's drop count with its time. **The links between views**
("Decided"): a selection carried from one view to the next, `Enter` to go in,
`Backspace` to come back, and the `process` view registered as a stub with
the frame's choice of first screen. **The mouse** ("Decided"): every view
reports the regions it drew with their rectangles, the frame routes clicks,
wheel and back to them, the toggle key, and the snapshot harness able to send
a click at a cell so each view's click path is tested like its keys. The
mouse is decoded in SGR mode and works inside tmux with `mouse on` (the
user's setup: Ghostty and tmux); say in `docs/view/README.md` what a
terminal or tmux needs for it. `omn top` in `man/omn.1` (after
shared's) and `docs/GUIDE.md`. The re-recording is stream record's, in phase B:
draw your changes on the current fixtures; the `raw` the family streams add
arrives with them. You own shared layers: one
`make check`.

### workloads -- a program per view whose load is known

One program per view and one for `process` ("Decided"), under
`view/workloads/`, each with a manifest of what it does: per phase, per
thread, per function, per request, per file, per socket, per kernel -- the
durations, counts, bytes and causes a view can draw, with how exact each is
(a sleep is exact to the scheduler's slack; a byte count is exact). Python
like the existing workloads (the GPU one drives PTX through `libcuda`, no
toolkit), fixed seeds, fixed sizes, a few seconds each, nothing that needs
privilege. `tests/view_workloads.rs` runs each under `omn trace` with the
view's sources and checks the sources against the manifest -- the workload's
own ground truth, before any view draws it. The family streams run beside
you: check what `main` serves today, and leave the `raw` they are adding to
record's test.

### record -- the fixtures, and each view held to its workload (phase B)

**One re-recording** on merged phase A: a recording of each view's
workload (all of them on the GPU where they touch it), plus the realistic
ones from `cases/` -- the file server, a `vertical-abandoned` run, a few
hundred requests, a CPython run with its perf map, a system-scope
`sched.wakeup` run -- with the `raw` the family streams added. Every snapshot
redrawn. `tests/view_truth.rs`: each view, drawn on its workload's recording,
shows the manifest's numbers within their trust (the hottest function, the
request's cause, the bytes per file, the phase boundaries). The gallery
README rewritten, its `Keep?` column "yes (2026-10-04)".

### process -- one process at a glance (phase B)

The new view ("Decided"), drawn first on the `process` workload's recording
(record it yourself into `docs/view/gallery/process-run/`, as `lanes-gpu`
was): one tile per view's essence for the target process
-- its wall-clock bar and first suspect (timebar), its hottest function on and
off cpu (flame), its cost line by resource (ledger), its slowest request and
why (request, when it declares requests), its latency strip (heatmap), its
threads' activity strip (lanes), what woke it most (wake), its bytes in and
out (flows), its GPU share -- each a number and a colour, each opening its
view. Default screen with a target; held to the re-recorded fixtures like
every view, with its verdict line.

### docs -- documents that lag the code, and the flaky tally (items 10, 11, phase B)

`docs/CATALOG.md`'s cost column and sched section; `examples/proc-not-built.yml`'s
exit; `docs/GUIDE.md`'s latency session; `docs/unwind.md`'s perf-map line; the
bench harness's `judge_band` reading a dilation within the workload's
resolution as zero; `tests/cases.rs`'s `MODEL_BUDGET` and `ask_mcp` fallback;
the flaky tally's new entries in files nobody else owns, each fixed or its
cause shown.

---

## Exit criteria for the wave

- The full suite green on a quiet machine at the end of the wave, run once by
  the coordinator, the flaky tally aside.
- Items 2 to 11 landed or re-filed with what blocked them.
- Every view has a workload of known load, and shows its manifest's numbers
  within their trust; every snapshot redrawn on the new recordings; the
  views linked; `process` the first screen of a targeted `omn top`.
- The two open decisions proposed with numbers.
