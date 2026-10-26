# Brief — coverage: every backend the catalog declares, collected or refused by name

The twenty-sixth wave: **close the gap between what the catalog declares and
what the build collects**. The catalog names 238 backends for 123 sources;
146 have a collector. A source is usable only through a backend this build
collects (`docs/DECISIONS.md`, "When a source is usable"), so every unwritten
backend is a machine on which a source reads "out of reach" for the build's
reason, not the machine's.

**Not launched yet.** This brief is written ahead of the wave; the "Open
before launch" section below holds the decisions the user takes first.

It runs in **two phases**. **Phase 0 gives the collectors a shape**: the
architecture review of 2026-10-21 found the design sound above the
`Collector` seam (catalog as data, one `Factory` registry serving both the
dispatch and `omn list`, the resolver, a runner tested with fake collectors)
and the friction below it -- every eBPF collector repeats its own load,
attach, drain, counter deltas, filter and loss sentences, and nothing of
that runs without the capabilities. The wave adds about 55 collectors;
written in today's shape they would copy that cycle 55 more times. **Phase
1** is the family streams, which first move their family's existing
collectors onto phase 0's modules, then write this wave's backends in the
new shape.

Read `CLAUDE.md` first -- **"How much to test, and when", its section "In a
wave of parallel streams"** -- then this brief, then your stream's section.
The twenty-fifth wave's brief is `docs/dev/briefs/wave-25-all-at-once.md`.

---

## Where the numbers stand

On `main` after the twenty-fifth wave, `target/debug/omn list --format json`
with the capabilities on the binary:

| | Total | With a collector | Usable here |
|---|---|---|---|
| Sources | 123 | about 120 | 115 |
| Backends | 238 | 146 | 135 |

The 92 backends with no collector, by what they wait on:

| Kind | Count | What writing them buys |
|---|---|---|
| `bpf:tp`, `equivalent: true` behind a `bpf:raw_tp` | 38 | the same events where tracefs is readable (root) and a raw tracepoint is not served: a kernel without BTF, or before 5.8 |
| alternatives whose probes pass here, the source served by another backend | about 27 | the machines where that other backend is refused: `bpf:kprobe` (5), `bpf:fentry` (3), `bpf:raw_tp` (8), `fs:cgroup` (5), `netlink` (3), `fs:proc`, `pmu`, `bpf:uprobe` |
| hardware this machine lacks | about 17 | Intel PT and CoreSight, PEBS, uncore, RDT, `i915`/`xe`/`amdgpu` PMUs, ROCm, Level Zero, the AMD and Intel GPU sysfs |
| a probe not written (`unknown`) | about 10 | `rdt:mbm`, `xdp`, `SO_TIMESTAMPING` + PHC, USDT for `mem.heap.alloc` and `runtime.jit`, `sched.mutex`'s uprobe, `mem.wss` |

### What the distroproof cells say a collector would buy

Read from `target/matrix/cells/*/sweep.json` (2026-10-19 and 20): for every
source a cell lists out of reach, the first unwritten backend of its chain
whose every requirement reads available there.

| Source @ backend | Cells it would make usable (of 33) |
|---|---|
| `power.throttle` @ `bpf:raw_tp` | 19 |
| `power.freq` @ `bpf:raw_tp` | 18 |
| `power.cstate` @ `bpf:raw_tp` | 18 |
| `io.latency` @ `fs:proc` (per device, degraded) | 14 |
| `io.readahead` @ `bpf:fentry` | 9 |
| `io.fsync` @ `bpf:kprobe` | 6 |
| `mem.faults` @ `bpf:raw_tp` | 5 |
| `sched.lock` @ `bpf:kprobe` | 2 |
| `io.syscall` @ `bpf:raw_tp`, `io.fs`, `net.socket`, `net.tcp.connect` @ `bpf:kprobe` | 1 each |

That is +4 on Fedora, Arch and Gentoo (96 to 100), +6 or +7 on Debian 12,
Ubuntu and Rocky 9, and **+1 at most on the low cells**: Debian 10, Ubuntu
18.04 and 20.04 (36 to 39), every container (44 to 59) and lockdown (55).

**The low cells are not short of collectors; they are short of backends.** 42
sources declare eBPF backends only (`mem.page_alloc`, `mem.kmalloc`,
`mem.reclaim`, `mem.compaction`, `io.queue`, `io.size`, `io.vfs`,
`io.syscall`, `io.fs`, `io.pagecache`, `io.readahead`, `io.writeback`,
`io.fsync`, `net.tcp.connect`, `net.tcp.accept`, `net.tcp.lifetime`,
`net.tcp.retransmit`, `net.drops`, `net.socket`, `net.qdisc`, `net.softirq`,
`net.xdp`, `net.request`, `sched.wakeup`, `sched.runqlat`, `sched.offcpu`,
`sched.migrations`, `sched.lock`, `sched.futex`, `sched.irq`, `sched.slices`,
`sched.runqlen`, `sched.numa_balance`, `gpu.job`, `proc.syscall`,
`proc.calls`, `proc.exec`, `proc.exit`, `proc.signals`, `runtime.gc`,
`runtime.alloc`, `runtime.custom`). Wherever eBPF is refused -- before 5.8
without root, under lockdown, in any container -- all 42 go at once, and no
collector of a declared backend brings one back. The rest of the low cells'
losses are privileges (`perf_event_paranoid`, the container's seccomp) and
hardware the guests lack (13 GPU sources): neither is code.

Two catalog defects the sweep shows:

- **`kernel>=5.8` gates 36 backends** by version, and RHEL backports:
  `rocky-8` (4.18) refuses every `bpf:raw_tp` on the version alone. A
  version is not a feature.
- **`power.freq`, `power.cstate` and `power.throttle` on `bpf:raw_tp` read
  available in a guest** with no cpufreq, cpuidle or thermal driver: the
  tracepoint exists and will never fire. Usable that way is a source that
  attaches and says nothing.

---

## Settled before launch

Settled by the user on 2026-10-21:

- **Hardware this machine lacks waits for a machine that has it.** No
  collector is written for Intel PT, PEBS, uncore, RDT, Intel topdown,
  `thermal_throttle`, the `i915`/`xe`/`amdgpu` PMUs, ROCm or Level Zero in
  this wave: a collector that cannot be shown against ground truth here
  would be a source listed usable that fails on the machine it was written
  for. They stay declared, probed and refused by name, as `lbr` and
  `sframe` are (`docs/DECISIONS.md`, "`lbr` and `sframe`"). No `hw` stream.
- **Root cells on old kernels are added to the matrix.** It has no `root`
  cell below 6.1, the only place the 38 `bpf:tp` fallbacks serve. The
  coordinator adds and sweeps `debian-10-root`, `ubuntu-20.04-root` and
  `rocky-8-root` before the wave, so the counts there are the baseline,
  and again at its end, where those rows must show.
- **`net.xdp` stays vocabulary.** An XDP program can drop packets, not only
  observe them; the source keeps no collector and the listing says why.

---

## Decided before this wave

- **A degraded backend says what it loses** (standing invariant). A
  non-eBPF fallback that counts where the source traces, or averages where
  it gives a distribution, declares `degraded:` with that sentence, and the
  emits and keys it cannot carry are not in its row.
- **No backend for a source that has no honest one.** A source whose
  question needs a per-event program (`io.fsync`'s latency, `sched.offcpu`'s
  stacks, `sched.futex`, `net.request`) stays eBPF-only; the stream records
  why in `docs/scenarios/<f>.md`, and the listing's refusal names eBPF as
  the requirement. Each family stream decides source by source and the
  handoff carries the table.
- **"Available" means it fires.** A backend whose requirement is a driver
  (cpufreq, cpuidle, a thermal zone) requires the driver, not only the
  tracepoint.
- **Every gain is shown on a cell.** A stream shows its collector here; the
  coordinator's end-of-wave sweep is what shows the cells' counts moved,
  and the exit criterion is the table above met, every judge of
  `cargo test --test portability` green.
- **Append seams**, as in the twenty-fifth wave: `src/bpf/mod.rs`'s `mod`
  lines, `profile::SAMPLED` rows, `catalog/<family>.yaml`'s `default:` block.

---

## Phase 0 — the shape the new backends are written in

Six streams, in two steps: **0a** (`kinds`, `requires`, `poller`, `main`)
start together; **0b** (`kernel`, `runner`) start once `kinds` is merged.
Phase 1 starts when 0b is merged. **No behaviour changes in phase 0**: every
`family_*`, `scenario_*` and `cross_*` test passes unchanged, `omn list`'s
JSON is byte-identical before and after on this machine, and a stream that
cannot keep that says so before it lands.

The counts below are `main` at `39cdd50`; each stream's exit criterion is
its count brought down, measured the same way, and said in the handoff.

### 0a · `kinds` — the backend kind as a type, not a prefix

- **Today**: behaviour reads the spelling of a backend id.
  `resolve.rs:904` (`BpfOnly` targets) and `:1146` (pushed-down filter
  credit) test `starts_with("bpf")`; `collect/mod.rs:998`
  (`loads_programs`) and `InsideMeter::of` (around `:1143`) split on `:`;
  98 `CollectorId::new("<source>/<backend>")` restate the `Factory` key.
- **Lands**: the catalog loader classifies each backend once into a
  `BackendKind` (loads a kernel program, pushes filters down, how its
  inside cost is metered); every reader asks the kind. `Factory` derives the
  collector's id; the 98 literals go.
- **Exit**: no `starts_with("bpf")` or `split(':')` on a backend id outside
  the loader; no `CollectorId::new` with a literal.

### 0a · `requires` — requirements typed at load (absorbs phase 1's `probe`)

- **Today**: `catalog.rs:243` keeps `requires: Vec<String>`;
  `Capabilities::probe_requirement` (`capabilities.rs:962-1143`) is an
  ordered chain of `strip_prefix` over about twenty kinds, order
  significant (`kprobe` before `probe_type_support`), anything else
  `Probe::Unknown` at run time. Sub-parsers live in `bpf/mod.rs:317`,
  `fentry.rs:386`, `resolve.rs:1227`. Tracepoint prototypes are written
  twice: in the catalog and in each program's `*_SIGNATURE` constant
  (`bpf/runqlat.rs:60` and others), held together only by a test helper.
- **Lands**: the loader parses each `requires:` string into a
  `Requirement` enum and **refuses a kind it does not know** at load, not
  at the run; the probe is one exhaustive `match`. The YAML does not change
  -- the catalog stays data (`docs/DECISIONS.md`, "Catalog location"); only the
  parse moves. A program takes its tracepoint prototype from the parsed
  requirement rather than restating it. Then phase 1's `probe` items: every
  `kernel>=5.8` replaced by the feature it stands for, the `unknown` probes
  written, the "available means it fires" judge.
- **Exit**: no `strip_prefix` chain; no `*_SIGNATURE` constant duplicating a
  catalog prototype; `rocky-8` listed through feature probes.

### 0a · `poller` — one module for counters read at intervals

- **Today**: about twenty pollers each keep `previous`/`last`/`baseline`
  state, twenty match `Mode::{Snapshot, Count, Poll}` themselves, seven
  carry a first-tick flag, and six write their own `delta_event`
  (`io/diskstats.rs:383`, `mem/system.rs:288`, `power/energy.rs:164`,
  `power/energy_pmu.rs:249`, `power/cstate.rs:168`, `cpu/util.rs:110`);
  `net/procfs.rs:132`'s `since()` is shared by three files only.
- **Lands**: a counter poller module. A collector supplies one thing --
  read the named cumulative counters now -- and the module owns the
  baseline, the delta, wraparound and resets, the three modes and the
  series. Its test adapter is a sequence of hand-written readings, so the
  semantics are tested with no file read. Two pilots moved onto it:
  `io/diskstats.rs` and `net/snmp.rs`.
- **Exit**: the module, its tests on fabricated readings, the two pilots
  unchanged in output. Phase 1 moves the rest and writes this wave's
  `/proc/vmstat`, `snmp`, `softnet_stat`, `/sys/block/*/stat` fallbacks on it.

### 0a · `main` — logic out of the binary

- **Today**: `src/main.rs` holds 4 037 lines of code: the budget ladder and
  measured cost (855-1545), comparing backends (2181-2540), the `top` view's
  tap (3021-4000); `thread_cpu_ns` is duplicated (`main.rs:3990`,
  `collect/mod.rs:1274`); `weighs_its_samples` (`:937`) and `mod cli` (`:27`)
  are pass-throughs.
- **Lands**: the budget ladder and backend comparison in the library, with
  tests that call them; the tap beside the view; the duplicates and
  pass-throughs gone. `main.rs` is argument wiring and exit codes.
- **Exit**: `main.rs` under 1 000 lines of code; the budget ladder tested
  without the binary.

### 0b · `kernel` — one kernel collector module, and the attach kind as a context

- **Today**: `fn drain_into` ×36, `fn absorb_drops` ×21, `filtered_out +=`
  ×32, `crate::filter::matches` ×35, `fn load(` ×39 (nine of them the same
  `RAW_TRACEPOINT` wrapper), `const LICENSE` ×23, "the kernel skipped a
  run of this program" ×15, `check_signature` ×6, `raw_tracepoint_open` in
  50 calls over 30 files. raw_tp, tp, kprobe and fentry are four attach
  paths (`bpf/attach.rs:359,410`, `bpf/fentry.rs:928`); one program,
  `proc.calls`, reads its arguments through a context
  (`bpf/proc_calls.rs:197`, `enum Context { Raw, Classic }`). `attach.rs`
  mounts five programs through `#[path]` (`:62-71`), so raw_tp programs
  reach `attach::fentry::skipped`. The eBPF collectors hold concrete
  programs, so their loss and filter logic cannot be tested without
  `CAP_BPF`.
- **Lands**:
  - A **kernel collector** module that owns the cycle: load, attach,
    detach, drain, cumulative counter deltas, the filter applied
    user-side, the loss sentences, skipped runs, program statistics. A
    collector declares its maps, its instructions and its rows-to-events
    conversion. Two adapters behind the seam: the kernel (`bpf()`), and an
    in-memory one fed with rows and counters, which is what the unit tests
    use -- loss accounting and filtering tested at uid 1000.
  - **The attach kind as a context adapter**: `proc_calls`'s `Context`
    generalised, so a program reads "argument n" or "field x" and the
    adapter turns that into BTF offsets (raw_tp, tp_btf, fentry), a
    tracefs record layout (tp) or registers (kprobe). One program body
    serves every attach kind of its chain; an `equivalent: true` backend is
    one more adapter, not one more program.
  - The `#[path]` mounts replaced by plain modules; `skipped` where it
    belongs.
  - Two pilots, chosen to exercise both: `sched.runqlat` (three raw
    tracepoints, cumulative drop counters) and `io.latency` (ring, per-cpu
    rows, `tp_btf`), each also served on `bpf:tp` through the context.
- **Exit**: the pilots on the module with their family and scenario tests
  unchanged, their `bpf:tp` rows collected and shown as root by the exact
  command in the handoff, the in-memory adapter's tests green without the
  capabilities. A full suite run: this is a shared layer.

### 0b · `runner` — one entry point

- **Today**: `collect/mod.rs` is 5 435 lines of code with fourteen concerns
  (errors, losses, inside accounting, budget, recorder, traits and
  registry, run entry points, orchestration, slots, spending, pace and
  drive, ledger, recording); its interface has seven entry points
  (`run_all`, `run_all_within`, `run_all_within_unserved`,
  `run_all_bounded`, `record_within`, `record_bounded`,
  `run_all_escalating`). `join.rs`'s decomposition (2841-4213) is a second
  engine inside the join engine.
- **Lands**: one entry point taking the run's options; the concerns split
  into files under `collect/` (a split of files, the interface the one
  entry point); the decomposition its own module beside `join`.
- **Exit**: one public run entry point; no file under `collect/` over 2 000
  lines of code. Starts after `kernel` is merged if the two conflict in
  `collect/mod.rs`; the coordinator decides at 0b's start.

---

## The machine

AMD Ryzen 9 3900X (Zen 2: `ibs_op` and `ibs_fetch`, no topdown PMU), 24
cpus, 62 GiB, RTX 2080 Ti, kernel 7.2.9, `perf_event_paranoid` 2. `omn`
carries `CAP_BPF` and `CAP_PERFMON` after `make`; `sudo -n` grants nothing
else. Never ask for a password, never run `sudo` without `-n`.

To show a fallback on this machine, force it: `--backend force:<id>` runs
the backend the resolver would pass over. A kprobe road for a source served
here by fentry is shown that way, against the fentry collector's own count
on the same workload.

---

## How you test

As the twenty-fifth wave: **only what you produced** -- the touched module,
`tests/family_<f>.rs`, `tests/scenario_<f>.rs`, your new test file, then
`cargo clippy --workspace --all-targets -- -D warnings` and
`cargo fmt --check`. No `make check`, no `cargo test --workspace`, unless
the stream changes a shared layer. **Every phase 0 stream does**: each runs
the full suite once, at its end, before it hands off -- six runs on a
machine carrying at most four streams, which is what a shared layer costs.
Phase 1 streams are back to their own files. `make` once
per build, before an eBPF test. Never `scripts/matrix.sh`: the sweep is the
coordinator's, once, at the end.

## Where you work, and how you hand off

**Run one stream at a time, by one agent, in the worktree
`/home/moha/dev/omniscient-claude`** (the user's standing preference since
the twenty-fifth wave: parallel streams used up the token budget). The
phases and their order stand; "streams" below are steps, each merged into
`main` before the next starts. The per-stream worktree lines that follow
are kept for a wave the user chooses to run in parallel.

- A worktree per stream, `/home/moha/dev/omniscient-w26-<stream>`, branch
  `feat/w26-<stream>`, from `main`. Never edit the main checkout, never
  push, never open a pull request; never `git stash`.
- A stream owns the files of its row and nothing else; a change elsewhere is
  filed under "Needs from another stream".
- Hand off in `docs/fixes/w26-<stream>.md`: what landed, shown vs reasoned
  with the numbers, each source's decision (new backend, or eBPF-only and
  why), cells expected to move, decision lines for `docs/DECISIONS.md`.

| Phase | Stream | Owns |
|---|---|---|
| 0a | kinds | `src/catalog.rs`'s backend classification, the `BackendKind` readers in `src/resolve.rs` and `src/collect/mod.rs`, the `CollectorId` literals in every `src/collect/` file (one mechanical commit, said first) |
| 0a | requires | `src/capabilities.rs`, `src/catalog.rs`'s requirements, the sub-parsers in `src/bpf/mod.rs`, `fentry.rs` and `resolve.rs`, `tests/portability.rs`'s judges, the `kernel>=` lines of every `catalog/*.yaml` |
| 0a | poller | a new `src/collect/poll.rs`, `src/collect/io/diskstats.rs`, `src/collect/net/snmp.rs`, `src/collect/net/procfs.rs` |
| 0a | main | `src/main.rs`, the new library files it moves code into, `view/`'s tap |
| 0b | kernel | a new `src/bpf/collector.rs` (or `src/collect/kernel.rs`), `src/bpf/attach.rs`, `src/bpf/{runqlat,io,proc_calls}.rs`, `src/collect/sched/runqlat.rs`, `src/collect/io/block.rs`; `insn.rs`, `map.rs`, `ring.rs` as shared layers |
| 0b | runner | `src/collect/mod.rs`'s runner, its split under `src/collect/`, `src/join.rs`'s decomposition |
| 1 | cpu, io, mem, net, power, proc, sched | `catalog/<f>.yaml`, `src/collect/<f>/`, the family's `src/bpf/` programs, `tests/family_<f>.rs`, `tests/scenario_<f>.rs`, `docs/scenarios/<f>.md` |
| 1 | perftp | a new `src/collect/perftp.rs` and its tests |
| 1 | tp | gone into `kernel`: a `bpf:tp` row is one context adapter, written by the family that owns the program |

---

## Phase 1 — the streams

**Every family stream starts by moving its family's existing collectors**
onto phase 0's modules: each eBPF collector onto the kernel collector
module, each poller onto the counter poller, with its tests unchanged and
one commit per collector. Only then does it write this wave's backends, in
that shape. A collector that does not fit the module is filed in the
handoff with why, rather than bent; that is what the module's next round
reads. The wave's exit adds: `drain_into`, `absorb_drops` and
`fn load(` counted again, and each down to the module's own.

- **power**: `power.throttle`, `power.freq` and `power.cstate` on
  `bpf:raw_tp` (19, 18 and 18 cells), each requiring the driver its
  tracepoint fires from. `power.throttle` is the one source unusable here
  that a written backend fixes: show it on this Ryzen against the thermal
  zones' own trip counts, or say why it cannot fire here.
- **io**: `io.latency` on `fs:proc` (`/proc/diskstats`, per device: 14
  cells), `io.readahead` on `bpf:fentry` (9), `io.fsync` and `io.fs` on
  `bpf:kprobe` (6 and 1), `io.syscall`, `io.bytes`, `io.iops`, `io.merges`
  on `bpf:raw_tp`, `io.bytes` on `fs:cgroup`, `io.latency` on `fs:tracefs`.
  Non-eBPF fallbacks to decide: `io.queue` and `io.size` from
  `/sys/block/*/stat` (averages), `io.writeback` from `/proc/vmstat` and
  `/proc/meminfo` (counts).
- **mem**: `mem.faults` on `bpf:raw_tp` (5 cells), `mem.heap.live` on
  `fs:proc`, `mem.wss`'s two `fs:sys` roads and their probe. Non-eBPF
  fallbacks to decide: `mem.page_alloc`, `mem.reclaim`, `mem.compaction`
  from `/proc/vmstat` (counts, no task); `mem.kmalloc` from
  `/proc/slabinfo` (root, per cache).
- **net**: `net.socket` and `net.tcp.connect` on `bpf:kprobe`, `net.bytes`
  and `net.tcp.rtt` on `bpf:fentry`, `net.bytes`, `net.packets` and
  `net.iface` on `netlink`. Non-eBPF fallbacks to decide:
  `net.tcp.retransmit` from `/proc/net/snmp` and `netstat` (counts),
  `net.drops` from `/proc/net/softnet_stat` and `snmp` (no reason),
  `net.softirq` from `/proc/softirqs`, `net.tcp.lifetime` and
  `net.tcp.accept` from `sock_diag` polled (sampled, misses short
  connections). `net.hwts`'s per-packet `SO_TIMESTAMPING` and its PHC probe.
- **sched**: `sched.lock` on `bpf:kprobe` (2 cells), `sched.mutex` on
  `bpf:uprobe`. Non-eBPF fallbacks to decide: `sched.runqlat` from
  `/proc/<pid>/schedstat` (total run delay, no distribution),
  `sched.migrations` on `pmu:sw`, `sched.irq` from `/proc/interrupts` and
  `/proc/softirqs` (counts, no time).
- **proc**: non-eBPF fallbacks to decide: `proc.exec` and `proc.exit`
  through the proc connector (netlink, `CAP_NET_ADMIN`) or `/proc` polled
  (misses short-lived tasks, said); `proc.syscall` has none worth its name.
- **cpu**: `cpu.oncpu_time`, `cpu.util` and `cpu.pressure` on `fs:cgroup`;
  `cpu.profile`'s `pmu` row stays the pointer it was decided to be.
- **probe**: moved into phase 0's `requires`. Its items: every
  `kernel>=5.8` replaced by the feature it stands for (a raw tracepoint
  attach, BTF, a helper), probed; `rocky-8` is the cell that shows it. The
  `unknown` probes written: `rdt:mbm`, `xdp`, `phc`, the USDT sites of
  `mem.heap.alloc` and `runtime.jit`. A judge in `tests/portability.rs`: no
  backend reads available in a cell whose run of it attaches and records
  nothing for a workload that must fire it.
- **perftp**: a counting backend through `perf_event_open` on
  `PERF_TYPE_TRACEPOINT`, no eBPF: the one generic road for the 42
  eBPF-only sources' **counts** under lockdown and in a container with
  `CAP_PERFMON`. It needs the tracepoint's id from tracefs, so show first,
  on `fedora-latest-lockdown`'s configuration and a capadd container's,
  whether it opens at all; the family streams add its rows only if it does.
- **tp**: no longer a stream. Phase 0's `kernel` makes a `bpf:tp` row one
  context adapter of a program `bpf:raw_tp` already loads; each family
  writes its own rows while it moves its collectors, whatever the root
  cells show, since the cost is now small.

Not this wave: `mem.slab` and `net.capture` are privileges, not code
(`/proc/slabinfo` is root's, `make CAPTURE=1` grants `CAP_NET_RAW`); the
low cells' perf refusals are `perf_event_paranoid` and podman's seccomp,
which the listing already names.
