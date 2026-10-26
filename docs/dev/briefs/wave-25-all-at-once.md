# Brief — what the twenty-fourth wave left, all at once

The twenty-fifth wave: **`docs/dev/STATE.md`'s "After the twenty-fourth wave", item 1
and the code of item 4, all at once**. The handoffs of the twenty-fourth wave
(`docs/fixes/w24-<stream>.md`) left "Not done" items and requests one stream
made of another; `docs/dev/STATE.md` lists each by the file it lands in. This wave
lands them, split by that file.

It runs in **one phase**: eighteen streams start together. Each is merged as
soon as it is done; the full suite runs once at the end, by the coordinator,
on a quiet machine. Item 2 (what the user grants), item 3's look at a
terminal and item 5 (the distributions) are not this wave's.

Read `CLAUDE.md` first -- **"How much to test, and when", its section "In a
wave of parallel streams"** -- then `docs/dev/STATE.md`, "After the twenty-fourth
wave", item 1, the bullet(s) of your stream, then your section here.

**This is the only brief.** What comes after this wave is written in
`docs/dev/STATE.md` under "What remains", and nowhere else. The twenty-fourth wave's
brief is `docs/dev/briefs/wave-24-found.md`.

---

## Decided before this wave

- **Defects first in every stream**: a defect breaks a standing invariant or
  a number and is fixed before a feature in the same stream starts. A stream
  that cannot finish its list lands what it can show and files the rest in
  its handoff, ranked.
- **Every number a stream adds is shown on this machine** against something
  independent of `omn` (a workload of known shape, `/proc`, a reference tool
  through the runner), not reasoned. Reasoned-only is said as such.
- **`cpu.profile`'s `pmu` row** (the open decision): it stays, as a pointer
  to `cpu.cycles -m sample`, with that said in its caveat. Stream cpu writes
  the caveat; the coordinator writes the `docs/DECISIONS.md` line.
- **Append seams.** Places several streams must touch, each by appending
  only, never rewriting what is there; the merge keeps both sides:
  - `src/request.rs`'s `KeyKind` (a family adding a key it fills: the variant,
    its parse and display arms, its doc line). Stream emit owns the rest.
  - `src/bpf/mod.rs`'s `mod` lines.
  - `profile::SAMPLED` rows, and `catalog/<family>.yaml`'s `default:` block
    (the family stream owns the rest of its file).

---

## The machine

This checkout is on a new machine: x86_64, 24 cpus, 62 GiB, an RTX 2080 Ti,
kernel 7.2, `perf_event_paranoid` 2. **`sudo -n` grants nothing here** (no
sudoers line yet): `make` builds and says `omn` has no capabilities, and the
witnesses that want root skip. So the eBPF tests skip, loudly; a stream whose change
is an eBPF program then shows it through what does run (the verifier-free
unit tests, the program's emitted instructions, `/proc`) and says plainly
in its handoff that the attach was not run here, with the exact command for
a person to run. Never ask for a password, never run `sudo` without `-n`.

---

## How you test

- **Only what you produced**: the touched module (`cargo test --lib
  <module>`), your family's `tests/family_<f>.rs` and `tests/scenario_<f>.rs`,
  your own new test file; then `cargo clippy --workspace --all-targets -- -D
  warnings` and `cargo fmt --check`. **No `make check`, no `cargo test
  --workspace`.** A stream that changes a shared layer (emit, narrow, the
  `src/perf.rs` and `src/catalog.rs` exceptions below) also runs the
  integration tests that read it (`cli`, `aggregation`, `join_engine`,
  `encoders`, `recorder` for emit), still not the workspace.
- `make` once per build, before an eBPF test or a reference pair; never
  `sysctl`, a module, GPU clocks, `scripts/matrix.sh` or `scripts/bench.sh`.
- **Eighteen streams load this machine at once.** A timing test red only
  under that load is rerun alone once and, if it holds, noted, not widened.
  A test outside your files that fails: rerun it alone once, then file it.

## Where you work, and how you hand off

- A worktree per stream, `/home/moha/dev/omniscient-w25-<stream>`, branch
  `feat/w25-<stream>`, already created from `main`, with an untracked
  `.cargo/config.toml` that turns incremental compilation off. Never edit
  the main checkout, never push, never open a pull request: the coordinator
  merges.
- **Never `git stash`**: the stash is shared by every worktree. Commit work
  in progress on your branch, in small commits in the repository's style
  (`fix(<area>): ...`, `feat(<area>): ...`, the subject a sentence about
  what the user sees).
- A stream owns the files of its row and nothing else (plus the seams
  above). A change elsewhere is filed in the handoff under "Needs from
  another stream", and the item is declared not done with that reason.
- Hand off in `docs/fixes/w25-<stream>.md`: what landed, shown vs reasoned
  (with the numbers), not done and why, privileged runs wanted with exact
  commands, decision lines for `docs/DECISIONS.md`, requests to other streams.
- Update the user-facing text your change makes stale in files you own
  (a family's `docs/scenarios/<f>.md`, the help strings); `docs/CATALOG.md`,
  `man/omn.1`, `docs/dev/STATE.md`, `docs/DECISIONS.md` and `docs/open-findings.md` are the
  coordinator's at the end.

| Stream | Owns |
|---|---|
| run | `src/cli.rs`, `src/main.rs`, `src/collect/launch.rs`, `src/collect/task.rs`, `src/collect/mod.rs`, `src/session.rs`, `tests/cli.rs`, `tests/runner.rs` |
| emit | `src/emit.rs`, `src/request.rs`, `src/event.rs`, `src/join.rs`, `src/format.rs`, `src/render.rs`, `src/validate.rs`, `src/filter.rs`, `tests/aggregation.rs`, `tests/join_engine.rs`, `tests/encoders.rs` |
| lazy | `src/lazy.rs`, `src/resolve.rs`, `mcp/`, `tests/lazy.rs`, `docs/lazy.md`, the `default:` blocks |
| narrow | `src/bpf/fentry.rs`, and the call sites of `Narrowing` in `src/collect/io/`, `src/collect/net/`, `src/collect/proc/` (one commit, `feat(fentry): ...`, said first in the handoff) |
| cpu, gpu, io, mem, net, power, proc, sched | `catalog/<f>.yaml`, `src/collect/<f>/`, the `src/bpf/` programs of that family (below), `tests/family_<f>.rs`, `tests/scenario_<f>.rs`, `docs/scenarios/<f>.md`; cpu also `src/perf.rs`; gpu also `src/catalog.rs` (the unit and `stacks:` only), `shim/` and `cupti/` |
| heap | `mem.heap.*`'s eBPF attach: `src/bpf/mem_alloc.rs`, `src/collect/mem/heap*`, a new `tests/heap_bpf.rs`; the `mem.heap.*` entries of `catalog/mem.yaml` |
| reference | `tests/reference.rs`, `tests/reference/`, `catalog/references/`, `docs/replaces/`, `.github/expected-skips.txt` |
| views | `view/`, `tests/view_*.rs`, `tests/view_fixtures/` |
| triage | `src/triage/`, `tests/triage.rs`, `tests/cases.rs`, `cases/`, `examples/` |
| unwind | `src/unwind.rs`, `src/stack.rs`, `catalog/unwind.yaml`, `tests/unwind.rs`, `tests/stack_capture.rs` |
| docs | `docs/triage.md`, `docs/cases.md`, `docs/unwind.md`, `docs/view/`, `docs/GUIDE.md`, `docs/bench.md`, `docs/how-to-use/` |

The `src/bpf/` programs by family: cpu `oncpu.rs`; io `io*.rs`; mem
`mem_*.rs` but `mem_alloc.rs` (heap's); net `net_*.rs`, `request_net.rs`;
proc `proc_*.rs`, `syscall.rs`; sched `sched_*.rs`, `runqlat.rs`,
`switch_count.rs`, `offcpu.rs`, `wakeup.rs`, `migrate.rs`.
`src/bpf/{insn,btf,map,ring,attach,mod}.rs`, `src/collect/psi.rs` and
`src/catalog.rs` beyond gpu's two lines are shared layers: a change there is
filed, unless a defect cannot be fixed otherwise, in which case it is one
small commit named `fix(<layer>): ...` and said first in the handoff.

---

## The streams

Each stream's list is its bullet in `docs/dev/STATE.md`, "After the twenty-fourth
wave", item 1, and the matching request in `docs/open-findings.md`, "Needs
from another stream -- the twenty-fourth wave". In short:

- **run**: a live `omn top` of the machine starving its pollers (a poll
  round that never runs counted somewhere, and the cause found -- the
  aarch64 fix in `8a72e51`/`f36d98c` may be it, measure); `omn latency` and
  `omn saturate` exiting 1 when their answer carries `failures`;
  `Tap::described` sending `Update::Described` on every change;
  `Beside::capacity` filled; the process cpu spent after a window closed in
  the cost line; the `--emit` help naming `summary@<attr>` and
  `hist@<attr>`, `--program-stderr`'s `FILE.<n>`.
- **emit**: the keys `op`, `level`, `mapping`, `user`, `object`; a request
  naming its value attribute; a summary that sums a named attribute per
  series (`-k syscall -e summary` as `strace -c` with errors); timeline and
  rate counting a value-less event as unmeasured; `filter.rs` reading an
  attribute named `kind` before the event's kind; an inclusive per-function
  share; a `pcap` encoder (the header bytes are net's).
- **lazy**: `omn list --format json` carrying the defaults and each
  backend's own `emits`; `omn-mcp` offering the defaults (`family` + `cost`
  through `omn::lazy::compose`); the resolver choosing a backend by cost
  band, and a low band the machine cannot keep said, not avoided.
- **narrow**: `Narrowing` with a descendants map, so `io.syscall`, `io.fs`
  and `net.socket` follow forked children; `omn io -- ./script.sh` shown
  reading the children's work.
- **cpu**: `PERF_SAMPLE_PERIOD` decoded so frequency-mode samples carry a
  `weight`; `cpu.profile`'s provenance (`perf_maps`, `executables`);
  `/proc/kallsyms` primed before the window (the `--pid` attach delay);
  `cpu.context_switches -e rate` over `enabled_ns`; an `insn:<func>+0x<off>`
  key; `func` in `cpu.profile`'s keys; `cpu.oncpu_time`'s stale caveat; the
  `mem.cache_*` rows in `profile::SAMPLED`; the `pmu` row's caveat.
- **gpu**: `bytes_per_second` and an entry-level `stacks: device` in
  `src/catalog.rs` (`DEVICE_STACKS` goes); `gpu.pcie`; launch facts as
  values; NVTX, `cuMemsetD8`, `cuModuleLoadData`, `cuCtxCreate`;
  `gpu.freq`'s three clock calls one reading; `gpu.throttle`'s below-clocks
  shares (a defect, `docs/open-findings.md`).
- **io**: a `kworker`-dispatched block request charged to the bio's
  submitter; `io.bytes` on `fs:proc` counting from before the window.
- **mem**: `mem.oom`'s victim; a per-cache slab; `mem.numa`, `mem.oom` and
  `mem.swap` listed usable but refused at the run (a defect: "unknown never
  counts as available").
- **heap**: the heap's eBPF attach (heap 2).
- **net**: a `bpf:raw_tp` road for `net.tcp.connect`; the captured header
  bytes for pcap; `net.tcp.lifetime --tid`; `net.tcp.accept` following a
  launched tree; `ss -s`; `net.tcp.lifetime` in the lazy `med` band.
- **power**: `power.cstate -e rate`; `power.wakeups --system`; the
  per-process energy estimate.
- **proc**: `proc.calls` on `bpf:tp` and paths for `open`, `openat2`,
  `execve`; `proc.exec`'s tree exact through `sched_process_fork`;
  `proc.tasks` beside `proc.threads` in the lazy `low` band.
- **sched**: `named_by` and `perf_maps` beside `sched.offcpu`'s stacks;
  `ppid` as a filter field and on `sched.switches`, `sched.migrations`,
  `sched.wakeup`, `sched.lock`; `sched.lock`'s caller and address; the
  wakeup-to-switch wait in `sched.slices`; a per-cpu `sched.runqlen` program
  on a 99 Hz cpu-clock event.
- **reference**: `perf-trace-count` skipping on perf's lost events; the
  pairs listed for what landed (`io.util`, `io.vfs`, `net.snmp`,
  `net.sockets`, `sched.interrupts`, `sched.runqueue`, `sched.slices`,
  turbostat, `pidstat -w`, `mpstat`, `top -b`, ...); the pairs written as
  `omn ... --system -- workload`; `perfrecord.yaml`'s `load-latency` `why`;
  `cpu.cycles.system`'s band with a slack independent of the idle rate.
- **views**: `Beside::capacity` and the `Lifetime` mirror in
  `view/src/model.rs`; flows' `dir:` fold; `usage::RAMP` above 3:1 on a
  light terminal and its test un-ignored; `ON LOCKS -58 lost`; the
  `process` suspect reading sleep where `sched.futex` holds the wait; flows'
  file -> device ribbon; a placed drop on screen.
- **triage**: `make-sqlite`'s critical path with `ppid`; `read_mcp`'s job by
  `ppid`; `sqlite3-durability`'s `every-commit-waits-for-the-disk`.
- **unwind**: `Unwinder::perf_map_file` and `executable` giving cpu its
  provenance lines; the exec re-read.
- **docs**: the documents item 1 names as the coordinator's, brought up to
  what `main` does today (`docs/triage.md`, `docs/cases.md`,
  `docs/unwind.md`, `docs/view/*.md`, `docs/GUIDE.md`, `docs/bench.md`'s
  `io.fsync` and `net.tcp.connect` rows). Only `cargo test --test cli`.
