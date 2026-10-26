# Brief — across the seams

The twenty-first wave: what the twentieth wave's streams asked of each other
(`docs/open-findings.md`, "Needs from another stream — the twentieth wave",
and each `docs/fixes/w20-*.md`'s section of that name), regrouped **by the
file that has to change**, so no request falls between two streams again;
and the `process` view redone as the user meant it. The wave after this one
compares `omn` with every tool it can replace.

It runs as **seven streams**. Five start at once; `collectors` starts when
`data` is merged; `record` starts when everything else is merged. Each stream
is merged as soon as it is done, reviewed while it is merged, and its review
fixes merged after.

Read `CLAUDE.md` first -- **"How much to test, and when", its section "In a
wave of parallel streams"** -- then your section here, then the requests it
names, by grep and line range.

**This is the only brief.** What comes after this wave is written in
`docs/dev/STATE.md` under "What remains", and nowhere else.

---

## Decided before this wave

- **The `process` view is the system view for one process** (the user,
  2026-10-04, after seeing the tile grid the twentieth wave shipped: "not what
  I imagined"). One page that shows, at a glance and at a high level, what one
  process consumes of everything; the detail lives in the dedicated pages.
  The agreed sketch:

  ```
   python3 hot.py  pid 41207 · 12 threads · 00:42 · omn costs 0.8 % cpu
   ─────────────────────────────────────────────────────────────────────────
   CPU      ▂▃▅▇█▇▆▇█▇▅▃▂▂▃▅▇██▇▆   2.31 cores   avg 1.84   peak 3.02   19 % of machine
   Memory   ▁▁▂▂▃▃▄▄▅▅▅▆▆▆▆▆▇▇▇▇▇   1.42 GB      +18 MB/s   peak 1.44 GB
   Disk     ▁▁▁▅▁▁▁▇▁▁▁▅▁▁▁█▁▁▁▅▁   R 12 MB/s    W 48 MB/s  p99 4.2 ms
   Network  ▃▃▃▃▄▃▃▃▃▄▃▃▃▃▄▃▃▃▃▄▃   ↓ 3.1 MB/s   ↑ 0.4 MB/s  0 retrans
   GPU      ▁▁▁▁▁▁▁▇█▇▁▁▁▁▁▁▁▇█▇▁   28 %         VRAM 2.1 GB
   Energy   ▃▄▅▆▇▆▆▇▇▆▅▄▃▃▄▅▆▇▇▆▅   ≈ 41 W       (estimated, machine share)
   Time     ████████████▓▓▓▓░░░░   cpu 61 %   queue 8 %   I/O 19 %   sleep 12 %
   ─────────────────────────────────────────────────────────────────────────
   ▶ suspect: disk I/O (fdatasync)                       Enter/click → detail
  ```

  One line per resource: a sparkline over the run, the current value, the
  mean, the peak, its share of the machine; the time line split by state
  with the suspect `omn latency` names. Minimal text; colour only for what
  dominates and for trust (dimmed with a loss or an error bar). A click or
  `Enter` on a line opens its page: CPU → `flame`, Disk → `heatmap` (latency)
  or `flows` (bytes), Network → `flows`, Time → `timebar`, the header's cost →
  `ledger`. It is the first screen of `omn top -- <program>` and of `--pid`;
  `system` stays the first screen without a target. The tile grid goes.
- **Usage is coloured by how full the resource is** (the user, 2026-10-04:
  the system view reads too grey beside the others). On `system` and
  `process`, a resource's sparkline, bar and figure take the classic gradient
  -- green when the resource is free, through yellow, to red when it is full
  -- by its share of the capacity that exists (cores, memory or the cgroup's
  limit, a device's busy time, a link's speed, the GPU's engines and VRAM).
  That is a measured fraction of a known bound, not a threshold someone chose;
  a resource with no known capacity (energy, bytes on a device of unknown
  speed) keeps one neutral hue and says why. Trust must not wash the colour
  out: a figure with a loss, a time error or low determinism keeps its hue and
  gets the trust mark beside it (its error bar, `~`, a dotted underline), so
  the screen is never grey because the data is honest. One palette in
  `view/src/trust.rs` and the frame, used by both views, readable on dark and
  light terminals and in 256 colours.
- **A collector can fail the run**: a `Collector` says when the tool broke
  the program it measured (the GPU's injection library killing its target),
  and `omn` then exits non-zero with that sentence. A lost read stays a loss.
- **A collector can publish fields**: `Collector::fields()` (default empty)
  copied into `Results::fields`, so what a caveat or a note says in a
  sentence a program can read as a number (`net.request`'s `never_ended`,
  `unborn`, `replaced`, `filtered_out`).
- **Direction is a key**: `KeyKind::Dir`, so `io.bytes` and `net.bytes` give
  one series per direction.
- **Open, proposed by stream run with numbers**: whether a launched program's
  budget denominator takes in the orphans `omn` adopted while they live.

---

## How you test, and how you read

- **Test what you produce**: your files' tests, `cargo clippy --workspace
  --all-targets -- -D warnings`, `cargo fmt --check`. No `make check`, no
  `cargo test --workspace` -- except `run` and `data`, which own shared
  layers: one `make check` before your last commit.
- A test outside your files that fails: rerun it alone once, then file it.
- `cargo fmt` before `make`; `make` once per build, before eBPF tests.
- Read by range. Seven agents share this machine: assert separations.
- No sysctl, no module, no GPU clocks, no `scripts/matrix.sh` or
  `scripts/bench.sh`; privileged runs go in the handoff with exact commands.
- Your worktree builds without incremental compilation (its untracked
  `.cargo/config.toml`; never commit it).

## Where you work, and how you hand off

`/home/moha/repos/omniscient-w21-<stream>` on `feat/w21-<stream>`; handoff
`docs/fixes/w21-<stream>.md` with the usual sections. A request to a file you
do not own is filed there, not made.

---

## The streams

| Stream | Owns | Starts |
|---|---|---|
| **run** | `src/main.rs`, `src/collect/{mod,launch,task}.rs`, `src/cli.rs`, `man/omn.1`, `tests/{budget,runner,cli}.rs` | at once |
| **data** | `src/emit.rs`, `src/encode.rs`, `src/request.rs`, `src/stack.rs`, `src/catalog.rs`, `src/validate.rs`, `src/resolve.rs`, `src/capabilities.rs` | at once |
| **process** | `view/src/views/process.rs`, `docs/view/process.md`, `view/workloads/process/**` | at once |
| **views** | `view/**` (the frame and `trust.rs` included) except `views/process.rs` and `workloads/**`, `docs/view/*.md` except `process.md`, `tests/view_{model,live,timebar}.rs` | at once |
| **docs** | `docs/CATALOG.md`, `docs/unwind.md`, `docs/bench.md`, `docs/scenarios/proc.md`, `docs/families/proc.md`, `examples/**`, `tests/{cases,bench_gpu,bench_bands}.rs` | at once |
| **collectors** | `src/bpf/{request,request_net}.rs`, `src/collect/{net,io,cpu}/**`, `catalog/{net,io,cpu}.yaml`, `tests/{scenario,family}_{net,io,cpu}.rs` | when data is merged |
| **record** | `tests/view_fixtures/**`, `docs/view/gallery/**`, `tests/{view_snapshots,view_truth,view_workloads}.rs` | when all others are merged |

### run -- the run's outcome, and what `omn top` opens on

- The failure channel ("Decided"): `Collector::failure()`, the runner
  carrying it to `main.rs`'s outcome, `omn` exiting non-zero with the
  sentence; the GPU's `counters::died_in_exit` answers it (one line in
  `src/collect/gpu/`, allowed).
- `assemble` (shared's diff in `docs/fixes/w20-shared.md`): only the sources a
  join or decomposition reads are cloned, and a query-tier rule is applied to
  the stored run, not here.
- `top_live`: open `process` first with a target and no `--view`; set
  `value_attr`, `row_stamp` and `drains` on the live `Results`.
- `--program-stderr FILE` in `src/cli.rs`, what `OMN_PROGRAM_STDERR` sets, both
  in `man/omn.1`, with `Lifetime::adopted_ended` and the subreaper in the
  stop's paragraph. A `docs/CATALOG.md` cost-cell test in `tests/cli.rs` (docs'
  request), holding each family table's Cost cell to the catalog's chain.
- Propose the adopted-orphans denominator decision with numbers.

### data -- what results carry

- `Collector::fields()` → `Results::fields` ("Decided"), serialized only when
  not empty.
- `KeyKind::Dir` ("Decided").
- `Results::stacks` keeping each frame's `NamedBy` (or the labels a perf map
  named, with the map's device and inode), and stack rows keyed by the
  request's keys.
- The executable of each task a result keys (`src/stack.rs`; `comm` is cut at
  15 bytes and `/proc/<pid>/exe` is gone after the run).
- `targets: {pid: "no: <reason>"}` in the catalog loader, and
  `target_rejection_reason` reading it instead of searching caveats; the
  `gpu.kernel` entry moved to it.
- Land as the coordinator's the gpu stream's out-of-row lines
  (`check_stack`, one `resolve.rs` call, one `capabilities.rs` field): read
  them, keep or rework them.

### process -- the system view for one process

The view in "Decided before this wave", replacing the tile grid: its
`regions` are the resource lines, `Enter` and a click open the line's page.
Its lines use the usage gradient and the trust marks ("Decided"), from the
palette views lands in the frame (until it is merged, use `view/src/trust.rs`'s
current colours and leave the hues to it). Its workload (`view/workloads/process/`) loads every resource in phases whose
values are known, and its manifest says them; draw it on the current
recordings and hand `record` the session it needs. Lines whose source the run
did not ask say so in place, dimmed, never zero.

### views -- what the views asked of each other

- `system`: the usage gradient and the trust marks ("Decided"), so it is no
  longer grey; the palette is the frame's, shared with `process`.
- `ledger`: a public reading of its cost lines and a line for blocked time
  that opens `timebar`; `flame`: a public reading of the hottest function.
- `heatmap`: a drop shaded in the column of the drain that read it
  (`Results::drains`).
- `timebar`: a `sched.futex` summary per task splitting a lock out of sleep.
- `system`: the network row asks `net.bytes` as `raw` (in apart from out
  survives the run); `io.bytes` as `raw`. `flows`: the file → device edge
  from `io.fs`'s `blk_devs` matched against `io.bytes`' keys, `io.syscall`
  as `raw,summary,hist`; the README needs table and each view's doc follow.
- `model.rs`: `Lifetime::adopted_ended`, `Overhead::one_time_cpu_ns`,
  `Results::fields`; `Undrawn::of` reading `fields` instead of sentences.
- A snapshot per view after a click (`snapshot::click` on `snapshot::find`),
  `--against` and focus snapshots through `snapshot::draw_with`.

### docs -- the documents and the bench's schedule

The docs requests of every w20 handoff: `docs/CATALOG.md` (`cpu.oncpu_time`'s
`bpf:raw_tp` row, `raw` on `power.energy`, `net.bytes`, `gpu.engine_busy`;
`net.socket`'s interface key; a `net.request` row; NVML cost cells; no
`vendor:ncu`), `docs/bench.md`'s `vendor:ncu` lines, `docs/unwind.md`'s
aliasing paragraph, `proc-not-built.yml`'s exit 3 or 1 in the proc docs,
`examples/mcp/approved/program.py`'s header (`program_stderr`),
`examples/vertical/server.c`'s `MAX_PLAN`, `tests/cases.rs` (`program_stderr`,
`MODEL_BUDGET` and the `budget is required` fallback removed),
`tests/bench_bands.rs`'s `cheaper_equivalents`, and the GPU bench's schedule
(`examples/bench/gpu.rs`, `tests/bench_gpu.rs`): a launch count per arm so a
replaying arm fits the step back into about fifteen minutes.

### collectors -- the fields the views and the cases wait for (after data)

- `net.request`: `rx_bytes`, `tx_bytes` and `holder_since_ns`
  (`docs/fixes/w20-net.md`'s exact layout), the collector test's record
  length, and its `never_ended`, `unborn`, `replaced`, `filtered_out` through
  `fields()`.
- `io.bytes` and `net.bytes` one series per direction on `KeyKind::Dir`.
- `io.bytes` polled: the baseline taken before the program is released, not
  at the first tick.
- `cpu.oncpu_time` on `fs:proc`: a closing read at the target's exit while it
  is a zombie, and the caveat saying a launched program's children are not
  counted on that backend.

### record -- the recordings, last

Re-record every fixture on the merged tree (`process`'s with its new session),
re-bless every snapshot, add `process` to the gallery with "yes
(2026-10-04)", and keep `tests/view_truth.rs` and `tests/view_workloads.rs`
holding each view to its manifest.

---

## Exit criteria for the wave

- Every w20 request landed or re-filed with the file that blocks it.
- `process` drawn as the sketch, opening each line's page, first screen with
  a target.
- `omn` exits non-zero when the tool broke its target.
- The full suite, run once at the end, green apart from the flaky tally.
