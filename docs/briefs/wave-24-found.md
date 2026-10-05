# Brief — what the comparison found, all at once

The twenty-fourth wave: **`STATE.md`'s "After the twenty-third wave", items 1,
3 and 4, all at once**. The comparison against the tools `omn` replaces found
defects, a run shape five families asked for, keys, sources and attributes;
the views have the user's remark and their open points; and the lazy mode
for people who are not experts is planned. This wave lands all of it, split
by the file each item lands in.

It runs in **one phase**: sixteen streams start together. Each is merged as
soon as it is done, reviewed while merged, its fixes merged after; the full
suite runs once at the end, by the coordinator, on a quiet machine. Item 2
(what the user grants) and item 5 (the distributions) are not this wave's.

Read `CLAUDE.md` first -- **"How much to test, and when", its section "In a
wave of parallel streams"** -- then your section here.

**This is the only brief.** What comes after this wave is written in
`STATE.md` under "What remains", and nowhere else.

---

## Decided before this wave

- **The lazy mode's spelling** (`DECISIONS.md`, settled at this brief, stream
  lazy writes the line): a **family name where a source is expected** --
  `omn trace cpu -- prog`, `omn trace mem --pid 1234`, `omn trace io
  --cost low -- prog`. A family name has no dot, so it never collides with a
  source. The default is **catalog data**, a `default:` block in the family's
  own `catalog/<family>.yaml` (a family owns its default the way it owns its
  sources), naming the requests it composes per cost band; the loader refuses
  a default naming a source, mode or emit the family does not have. The run
  prints the full request it composed (so the person learns the long form),
  then says what it chose, what it degraded and what it lost, like any run. A
  family with no safe default refuses with the reason and the sources to pick
  from. A default never runs a session from `examples/`; a session may name
  a family's default.
- **Everything the comparison asked is in scope**, defects first in every
  stream: a defect breaks an invariant or a number and is fixed before a
  feature in the same stream starts. A stream that cannot finish its list
  lands what it can show and files the rest in its handoff, ranked.
- **Every number a stream adds is shown on this machine** against something
  independent of `omn` (a workload of known shape, `/proc`, a reference tool
  through the runner), not reasoned. Reasoned-only is said as such.
- **Append seams.** Three places several streams must touch, each by
  appending only, never rewriting what is there; the merge agent keeps both
  sides:
  - `src/request.rs`'s `KeyKind` (a family adding a key it fills: the variant,
    its parse and display arms, its doc line). Stream emit owns the rest of
    the file.
  - `src/cli.rs` / `src/main.rs`: stream lazy adds the family-name dispatch
    at the one point a source name is resolved; stream run owns the rest.
  - `catalog/<family>.yaml`'s `default:` block: stream lazy appends it at the
    end of each family file; the family stream owns the rest.

---

## How you test

- **Only what you produced**: the touched module (`cargo test --lib
  <module>`), your family's `tests/family_<f>.rs` and `tests/scenario_<f>.rs`,
  your own new test file; then `cargo clippy --workspace --all-targets -- -D
  warnings` and `cargo fmt --check`. **No `make check`, no `cargo test
  --workspace`.** Stream emit changes a shared layer: it also runs the
  integration tests that read the results layer (`cli`, `aggregation`,
  `join_engine`, `encoders`, `recorder`), still not the workspace.
- `make` once per build, before an eBPF test or a reference pair (the
  capabilities); never `sysctl`, a module, GPU clocks, `scripts/matrix.sh` or
  `scripts/bench.sh`.
- **Sixteen streams measure this machine at once.** A timing test red only
  under that load is rerun alone once and, if it holds, noted, not widened.
  A test outside your files that fails: rerun it alone once, then file it.
- `sudo -n` grants exactly the witnesses in `/etc/sudoers.d/omniscient-
  witnesses` (`bpftrace`, the bcc tools, `tcpdump`, `turbostat`, `perf`) and
  the commands of `omniscient-privileged`; never ask for a password, never run
  `sudo` without `-n`.

## Where you work, and how you hand off

- A worktree per stream, `../omniscient-w24-<stream>`, branch
  `feat/w24-<stream>`, with an untracked `.cargo/config.toml` that turns
  incremental compilation off. Never edit the main checkout.
- **Never `git stash`**: the stash is shared by every worktree of this
  repository. Commit work in progress on your branch.
- A stream owns the files of its row and nothing else (plus the append
  seams above). A change elsewhere is filed in the handoff under "Needs from
  another stream", and the item is declared not done with that reason.
- Hand off in `docs/fixes/w24-<stream>.md`: what landed, shown vs reasoned
  (with the numbers), not done and why, privileged runs wanted with exact
  commands, decision lines for `DECISIONS.md`, requests to other streams.
- Update the user-facing text your change makes stale in files you own
  (a family's `docs/scenarios/<f>.md`, the help strings); `CATALOG.md`,
  `man/omn.1`, `STATE.md`, `DECISIONS.md` and `docs/open-findings.md` are the
  coordinator's at the end (stream lazy writes its one `DECISIONS.md` line).

| Stream | Owns |
|---|---|
| run | `src/cli.rs`, `src/main.rs`, `src/collect/launch.rs`, `src/collect/task.rs`, `src/session.rs`, `tests/cli.rs`, `tests/runner.rs` |
| emit | `src/emit.rs`, `src/request.rs`, `src/join.rs`, `src/format.rs`, `src/render.rs`, `src/resolve.rs`, `src/validate.rs`, `src/filter.rs`, `tests/aggregation.rs`, `tests/join_engine.rs`, `tests/encoders.rs` |
| cpu, gpu, io, mem, net, power, proc, sched | `catalog/<f>.yaml`, `src/collect/<f>/`, the `src/bpf/` programs of that family (below), `tests/family_<f>.rs`, `tests/scenario_<f>.rs`, `docs/scenarios/<f>.md` |
| reference | `tests/reference.rs`, `tests/reference/`, `catalog/references/`, `docs/replaces/`, `.github/expected-skips.txt` |
| views | `view/` except `view/src/views/system.rs`, `view/src/views/process.rs`, `view/src/trust.rs`; `tests/view_*.rs`, `tests/view_fixtures/` |
| top | `view/src/views/system.rs`, `view/src/views/process.rs`, `view/src/trust.rs`, new `tests/view_top_*.rs` |
| lazy | new `src/lazy.rs`, new `tests/lazy.rs`, new `docs/lazy.md`, `src/catalog.rs` (the `default:` block only), the seams above |
| unwind | `src/unwind.rs`, `src/stack.rs`, `catalog/unwind.yaml`, `tests/unwind.rs`, `tests/stack_capture.rs` |
| triage | `src/triage/`, `tests/triage.rs`, `tests/cases.rs`, `cases/`, `examples/` |

The `src/bpf/` programs by family: cpu `oncpu.rs`; io `io*.rs`; mem
`mem_*.rs`; net `net_*.rs`, `request_net.rs`; proc `proc_*.rs`,
`syscall.rs`; sched `sched_*.rs`, `runqlat.rs`, `switch_count.rs`,
`offcpu.rs`, `wakeup.rs`, `migrate.rs`. `src/bpf/{insn,btf,map,ring,attach,
fentry,mod}.rs`, `src/perf.rs`, `src/collect/mod.rs`, `src/collect/psi.rs`
and `src/catalog.rs` (beyond lazy's block) are shared layers: a change there
is filed, unless a defect cannot be fixed otherwise, in which case it is one
small commit named `fix(<layer>): ...` and said first in the handoff.

---

## The streams

The numbers in parentheses are the comparison's gaps
(`docs/fixes/w22-<family>.md`, `docs/fixes/w23-<family>.md`); `STATE.md`,
"After the twenty-third wave", item 1, has each in a line.

### run -- the run's shape

1. **A machine-wide target bounded by a launched program**: `--system`,
   `--cpu`, `--cgroup`, `--iface` and `--pid` beside `-- prog`, the window
   the program's life, the way `perf stat -a -- prog` and `turbostat prog`
   measure (perfstat 3, power 1, io 3, top 6, net).
2. **Several sources on one launch** (`-e a -e b -- prog`, or the request
   list), one window for all (perfstat 2, top 5).
3. **`-r N`**: one request repeated, the mean and spread per value in the
   results (perfstat 5).
4. **Exit 1 saying which**: the machine refused, or a run failed before
   writing (reference).
5. **`--pid` ending when its process exits**, as `perf stat -p` does;
   `-m count --pid` without `-d` served (perfstat, top).
6. **`omn trace --pid` attached in under 500 ms**, or the run saying when it
   attached (perfrecord).
7. **The launched program's exit status in `results.json`** (runner).
8. **The launched tree**: the launcher's `PATH` search not counted as the
   program's `execve`s (perfsched 2); the closing round taken before the
   reap, so `cpu.oncpu_time` on `fs:proc` keeps its last interval (sysstat
   9); **the machine's capacity in a recording** (cpus, memory), so a replay
   colours by fullness.

### emit -- the results layer

1. **Defect first**: a filter on a field the resolved backend never fills is
   refused before the run with the backend's name, not counted after it
   (`cpu.profile` on `pmu:sw`, perfrecord 1); `mem.heap.alloc` refusing
   `stack` before the run, not after (heap 1) -- the refusal in the
   resolver, the field list from the backend.
2. **A total in `summary`** (heap 4, top 1; net: a counter's total is mean x
   count today) and **`summary` and `hist` per key** (`@kernel`, gpu 2).
3. **`topk`** with a zero series rather than none, and no `-0.0` (perfsched 5).
4. `summary` over `fs:proc` keyed `dir` alone not folding devices into one
   series (io 5).
5. The key kinds the families ask that need more than an appended variant
   (a key over a stack frame: function and object), coordinated in your
   handoff with the family streams' seam lines.

### cpu

`cpu.util` and `cpu.pressure`'s leading event of a few ms after the poll
starts (sysstat 2, **defect**); `cpu.branches` (perfstat 1); task-clock as a
source (perfstat 4); the per-state cpu split (top 9); a cycles `cpu.profile`
and sampling any PMU event (perfrecord 2, 6); function and object keys on
`cpu.profile` (perfrecord 3-4, with emit); `pid` per sample on `cpu.profile`;
instruction attribution (perfrecord 7); `cpu.pressure`'s attributes named as
its siblings' (sysstat 3).

### gpu

`gpu.mem` following the launched program's descendants (gpu 1, 0 under a
wrapper, **defect**); `gpu.freq`'s two lost records in the GPU bench
(`docs/open-findings.md`, "The GPU bench rerun alone", **defect**); the NVML
pollers' cost (25-36 % in the backends bench) down or its band honest;
summary and hist per `@kernel` (gpu 2, with emit); NVTX, launch facts, the
memory clock as a value, per-process engine busy, PCIe, throttle and ECC,
more driver calls, a CUPTI callback backend (gpu 3-9). The device is one:
run your GPU tests with `--test-threads=1`, and check `nvidia-smi
--query-compute-apps` first; another stream's GPU run beside yours is a
reason to rerun once, not a defect.

### io

`io.latency` following a forked program's descendants (io 1, 0 requests for
a forked fio, **defect**); a block request a `kworker` dispatched; `dir` as a
key on `io.latency` (io); `%util` (io 4); VFS calls by operation (bpftrace
7); `io.fsync`'s 144 % band in the backends bench, moved or explained; the
two caveats in `catalog/io.yaml` (a swap on `zram` is read I/O `/proc/<pid>/io`
charges and neither `io.latency` nor `io.bytes` sees; on a multi-device
filesystem a filter on the mount's source undercounts).

### mem

`mem.latency`'s IBS sampler delivering about 58 % of the rate asked without
saying so, and calling 18 % of its timed loads `peer` where `perf mem` calls
6 % a cache hit (perfrecord, **defect**); `level` as a key on `mem.latency`
(perfrecord 5); `mem.system` from `/proc/meminfo`, slab, the machine's NUMA
counters (sysstat 4-8); per-process swap-in; `mem.oom` (bpftrace 5); mapping
and user keys on the memory sources (top 4, 8); VIRT and SHR (top 3); the
heap's eBPF attach, high-water mark, live blocks and lifetimes (heap 2-3,
5-6); a caveat on `mem.faults`' earlier open (perfstat 6); `c2c`'s per-line
detail (perfrecord 8).

### net

`net.tcp.lifetime` per launch (bpftrace 3); `net.bytes`' polled baseline;
`dir` on `net.packets`; `net.snmp`, a socket inventory, pcap output,
`tcp_info`'s other fields, `missed` and `mcast`, `direction` as a field
`net.capture`'s `--filter` can name (`lo` hands every frame twice);
`net.tcp.accept`; attributes on `net.drops` (bpftrace 6);
`net.tcp.connect`'s 40 % band in the backends bench, moved or explained.
`make CAPTURE=1` grants `CAP_NET_RAW`; a plain `make` drops it.

### power

`power.freq`'s first poll taken at window open, before anything launched
beside it (**defect**); `power.freq`'s busy share and average, an idle share,
`power.freq` on an idle cpu saying what it reads (power 2-3); the core energy
domain (power 4); per-process wakeups (power 6).

### proc

`proc.syscall`'s `comm` filter reading the comm an aggregate first saw
(bpftrace 1, **defect**); `syscall` as a key on `proc.syscall`, so one run
answers `strace -c` (perfsched 1); `comm` as a key (bpftrace 4); `proc.exec`
per launch (bpftrace 3); syscall errors and per-call records (perfsched
3-4); attributes on `proc.syscall`'s `openat` and on `proc.signals`
(bpftrace 6); **every thread of a pid on `fs:proc`, and the machine's
processes as one source** (top 7) -- stream top's process table reads it, so
land it early and say its name in a commit message; `ppid` on the sched
sources is sched's.

### sched

`sched.irq --filter 'kind == "hard"'` returning no series (bpftrace 2,
**defect**); `sched.switches`' counted rate divided by a 12 to 22 ms window
(sysstat 1, **defect**, held by `pidstat -w`'s `known_defect`: when it
holds, the pair's marker fails, and that is stream reference's to remove);
a summary over `sched.runqlat`'s `wake_to_run_ns` (perfsched 8, with emit);
lock kind and caller keys on `sched.lock` (perfsched 6); `sched.runqlen`,
load and the run queue, interrupt counts (sysstat 5-7, bpftrace 5); a
per-switch stream (perfsched 9); `ppid` on the sched sources; the
`process` suspect reading sleep, not lock, on its workload, if the cause is
in the sched collectors.

### reference -- the runner and the pairs

1. **The runner** (`tests/reference/runner.rs`): a root check by command
   (`sudo -n` grants `/usr/bin/tcpdump`, not everything, so net's tcpdump
   pair skips today); a `teardown:` argv (perfstat's cgroup stays behind);
   under `omn_launches_tool`, `workload_fails_alone` running `command` +
   `workload` (io); "does not exist here" for a device path in `requires`
   (gpu).
2. **The pairs left**: `top-cpus` (three quiet runs recording which side is
   higher, then the bias named in `why` or `omn`'s window aligned to `top`'s
   first sample); `load-latency` (a `known_defect` naming the `mem.latency`
   population finding if it is red on a quiet machine); gpu's four
   `omn_*.json` captures recaptured with the driver (`w23-gpu.md`); with the
   witnesses granted now, bpftrace's two medians and perfsched's six `perf`
   pairs `alongside`, io's percentile pair (`omn_parse` with
   `log2_buckets`), a write pair beside io's three `iostat -dx` read pairs.
3. **The captures still written from documentation**, replaced by the real
   tools' output now that `sudo -n` grants them (each `docs/fixes/
   w23-<family>.md`, "Privileged runs wanted"): perfsched's
   `perf-trace-s.txt` and `perf-sched-latency.txt`, bpftrace's `<tool>.txt`
   beside each `.format.txt`, `memleak.txt`, power's `cpupower_mperf.txt`
   (still uid 0: skip with the reason) and `rapl_window*.txt`; each parser
   test's numbers moved to its capture.
4. A `known_defect` whose defect another stream fixes this wave fails once
   merged: that is the signal, and the marker goes. Read `main` before your
   last run.

Run families one at a time (three timing pairs go red when families run in
parallel), and not while a GPU stream holds the device.

### views -- every view fills the terminal

1. **The user's remark first**: every view fills the terminal the way
   `system` does, not drawn in a corner with the rest empty -- each view's
   layout scaled to the terminal's width and height, at 80x24, 160x48 and a
   wide 240x60, with snapshots at each.
2. Then: `Update::Described` in `view/src/feed.rs`; `Key::Dir` in the mirror;
   a recording holding a drop; the request view's bytes; flows'
   socket-to-interface edge and file-to-device ribbon; `--against` and focus
   snapshots in the standard set; `sqlite-latency`'s `blocked` with its
   events; the `raw` panels `system` could keep (`cpu.util`, `gpu.mem`) and
   the ledger's `cpu.oncpu_time`; the usage gradient checked on a dark and a
   light palette and in 256 colours (snapshots, since no person looks).

### top -- the system view lists the machine's processes

`omn top`'s `system` view listing the machine's processes, sorted, the one
use of `top`, `htop` and `btop` no pair could hold (top 2) -- reading stream
proc's machine-wide process source once it is on `main` (merge `main` when
it lands; before it, build on what `fs:proc` serves today and say so); a
batch output (`omn top --batch`, text a pair could parse); `system` split by
`dir`; the `process` view filling the terminal, live and replayed, its lines
and pages; `process` on `trust::usage`, `ledger::reading` and
`flame::hottest`; the `process` suspect on a lock the timebar now splits.

### lazy -- a family and a target, nothing else

As decided above. The `default:` block's schema in `src/catalog.rs` with the
loader's refusals; `src/lazy.rs` composing the request from the family's
default and the cost band (`--cost low|med|high`, mapped onto the bands the
budget already uses -- read `src/emit.rs`'s and `omn-mcp`'s cost words, do
not invent a scale); a default for every family (cpu, gpu, io, mem, net,
power, proc, runtime, sched), each the minimum that answers that family's
first question, each shown on a workload of that family's shape and run on
this machine; the composed request printed before the run; a family with no
safe default refusing with the reason; `omn list` showing each family's
default; `docs/lazy.md` for a beginner; the `DECISIONS.md` line.

### unwind

`Unwinder::symbolizer` and `perf_maps` (`docs/open-findings.md`, the
twenty-first wave); the unwind share tests' aliasing; `DEVICE_STACKS` as
data.

### triage -- the triage and the cases

The triage's failure channel; `--program-stderr` with `--runs`; the
provenance; the job restricted to the followed child's parent; the cases:
`make-sqlite`'s critical path, `http-server-stdlib`'s slow client on the MCP
way, `sqlite3-durability`'s commits (`docs/open-findings.md`, the
twenty-first wave). A flag on `src/cli.rs` is stream run's: file it.
