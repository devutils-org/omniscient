# Brief — programs nobody wrote for it

The seventeenth wave: **ROADMAP phase 12**, and the sixteenth wave's open
filings, grouped by the files they land in. Every ground truth so far is a
workload written for the test. This wave takes programs that were not written
for Omniscient -- `sqlite3`, CPython, `http.server`, `gcc`, `make -j` -- and
holds the answer to *"why is this slow, for this objective"* against a cause
known independently of the tool.

Read `CLAUDE.md` first -- **including "How much to test, and when", which this
wave is held to** -- then `docs/dev/STATE.md` ("What remains", "After the sixteenth
wave"), then `docs/ROADMAP.md` phase 12, then your section here. The earlier briefs
are in `docs/dev/briefs/`; waves 15 and 16 had none of their own and ran the list
`docs/dev/STATE.md` left.

**This is the only brief.** What comes after this wave is written in
`docs/dev/STATE.md` under "What remains", and nowhere else.

---

## Decided before this wave

- **Phase 12 is worth doing** (the user, 2026-10-03). Case 6's project is the
  **sqlite amalgamation**, fetched once and cached outside the repository.
- **Perf-map symbols land first**, as their own stream, because without them
  the CPython case can only check that the verdict says it cannot see.
  A `/tmp/perf-<pid>.map` is a file the target wrote on this machine and asks
  nothing of anyone, so it is read under `--symbols local` (and therefore
  `debuginfod`), not behind a new rung.
- **The GPL-2.0 text ships**: `LICENSES/GPL-2.0-only.txt`, installed by
  `make install` and packed by `scripts/release.sh`.
- **The network namespace goes beside an interface's index**: the identity
  becomes `iface#<netns>:<index>`, and a join across namespaces is refused
  with its reason, never credited silently (`docs/open-findings.md`, "the
  network namespace beside the index").
- **One order for "unbuilt" and "missing"**: the chain's, on both paths, as the
  filing suggests (`docs/DECISIONS.md`, "Open"); the forced path changes to match.

Each of these moves out of `docs/DECISIONS.md` "Open" into the settled table, with
its reason, in the stream that lands it.

---

## How you test, and how you read

- **While you work**: only your module and your test files --
  `cargo test --lib <module>`, `cargo test --test <file>`. Never `make check`
  while iterating.
- **Before your last commit**: one `make check`. If red, one
  `cargo test --workspace --no-fail-fast` to see every failure together. A red
  test outside your paths that is in the flaky tally: rerun it alone once.
- **`cargo fmt` before `make`, never after.** `make` once per build, before
  eBPF tests.
- **Read by range**: `grep -n`, then the lines you need.
- **Six agents share this machine.** Assert the separation, not a threshold
  calibrated on a quiet one. Do not start `scripts/matrix.sh` or
  `scripts/bench.sh`.
- **Privileged commands**: `/etc/sudoers.d/omniscient-privileged` grants,
  without a password, exactly the commands in `docs/dev/STATE.md` "Needs privilege"
  (sysctl `perf_event_paranoid` 2/-1, `null_blk`, `tc netem` on lo, a nosuid
  tmpfs) and `dmsetup`/`losetup`. **Do not change a sysctl**: it is machine
  wide and the other streams' tests read it. List the privileged run you want
  in your handoff; the coordinator runs it alone.

## Where you work, and how you hand off

Each stream has its own worktree, `/home/moha/repos/omniscient-w17-<stream>`,
on `feat/w17-<stream>`. Commit there with `git commit`; never touch the main
checkout. The handoff is `docs/fixes/w17-<stream>.md`: what landed, what you
showed versus reasoned, what you could not do and why, the privileged runs you
want, and the requests to other files under "Needs from another stream". A
request to a file you do not own is filed there, not made.

---

## The streams

| Stream | Owns | Depends on |
|---|---|---|
| **symbols** | `src/stack.rs`, `src/unwind.rs` (the names half), `src/collect/runtime/jit_map.rs` (to share its parser), `tests/stack_capture.rs` | -- |
| **cases** | `cases/**`, `tests/cases.rs`, `docs/cases.md`, `cases/sqlite3-*` | -- |
| **engine** | `src/bpf/mem_stall.rs`, `src/collect/mem/stall.rs`, `src/collect/proc/status.rs`, `src/collect/mem/footprint.rs`, `src/collect/net/**`, `src/bpf/net_qdisc.rs`, `src/event.rs`, `src/join.rs`, `catalog/{proc,mem,net}.yaml`, `docs/keys.md` | -- |
| **core** | `src/emit.rs`, `src/resolve.rs`, `tests/cross_memory.rs`, `docs/portability.md`, `LICENSES/`, `scripts/release.sh`, `Makefile` (install), `README.md` (licence lines) | -- |
| **python** | `cases/cpython-*`, `cases/http-server-*` | symbols, cases |
| **build** | `cases/sqlite3-durability`, `cases/gcc-*`, `cases/make-*` | cases |
| **io** | `src/bpf/io.rs`, `src/bpf/request.rs`, `src/collect/io/{block,histogram}.rs`, `tests/scenario_io.rs`, `docs/scenarios/io.md` | engine, core (rebases on them) |

`docs/DECISIONS.md` and `docs/dev/STATE.md` are the coordinator's. A stream writes its
decision lines into its handoff, and the coordinator moves them.

### symbols -- a perf map is a symbol source

`cpu.profile` (and every stack the tool names) reads `/tmp/perf-<pid>.map`
when the target wrote one: an address in an anonymous executable mapping that
a map line covers takes that line's name, and the frame says where its name
came from (`perf-map`), as it says `elf`, `debuginfod` or nothing today. Only
`runtime.jit` reads the file now (`src/collect/runtime/jit_map.rs`), as a
stream of compilations: share the parser, do not write a second one. The map
is read at the end of the run as well as during it (a JIT writes lines as it
compiles). Ground truth: a test program that maps an executable page, writes a
perf map naming it, and burns there; the profile names the frame and the
reference is the map the test wrote. Then `python3 -X perf` on a pure-Python
loop, skipped loudly where `sys.activate_stack_trampoline` is absent.

### cases -- a case is data

ROADMAP phase 12, tasks 1 to 3. `cases/<name>/case.yml` holds: the program and
how it was obtained (package, build id, flags), how to fetch anything not on
the machine (into `$XDG_CACHE_HOME/omniscient/cases/`, never into the tree),
the injected cause, the reference's own account (a command whose output is
parsed by a named extractor), the objective asked, and the verdict expected.
`tests/cases.rs` discovers every directory and runs each case **both ways** --
`omn triage` as a person would, and `omn-mcp` as an agent would -- and asserts
the two name the same first suspect, that it is the known cause, that every
number the verdict cites agrees with the reference within the reference's
resolution, and that the verdict says how it knows (method, evidence level of
each join, unwind mode and the source of each name, what it could not see).

Adding a program must mean adding a directory: the extractors are a small
registry in `tests/cases.rs` keyed by name, and a case names one. A case whose
program, fetch or injection is unavailable skips with the machine's reason
first (the skip gate reads it). The injection never modifies the target
(boundary 5); the draft in `/home/moha/repos/omniscient-wave16-brief-draft.md`
lists six ways, each with its reference.

The pilot is **case 1, `sqlite3` stripped** (names through debuginfod; the
CPU cause a query whose plan scans instead of searching, reference
`EXPLAIN QUERY PLAN` and `.stats`; the off-CPU cause a second process holding
`BEGIN EXCLUSIVE` for a known time while the target waits in `busy_timeout`,
reference the holder's own clock). Write `docs/cases.md`: the format, the
extractors, and how to add one.

A wrong suspect is a finding against the method, the collector or the unwinder
that produced it: file it, with the case that shows it, and do not carry a
workaround in the case.

### engine -- the sixteenth wave's engine filings

From `docs/open-findings.md`, the sixteenth wave's sections:

- **A `last_ns` per stall row** (`src/bpf/mem_stall.rs`,
  `src/collect/mem/stall.rs`), so the 100 ms reclaim session credits its rows
  too ("a tighter enclosing window for a drained stall row"). A shared eBPF
  layer: run the full suite once before handing off.
- **Carry `proc`** (`src/collect/proc/status.rs`, `src/collect/mem/footprint.rs`
  and their catalog entries): "`proc` has no producer".
- **The network namespace beside the index**, as decided above:
  `KeyValue::iface_in(netns, index, name)`, the identity
  `iface#<netns>:<index>`, the namespace's inode from the kernel side in
  `net_qdisc.rs`, `capture.rs`, `hwts.rs`, this process's own for a named
  procfs read. Ground truth: the 2000 packets on an unprivileged namespace's lo
  (`unshare -rn`) are no longer credited to this one's.

### core -- the shared layers and the root

- **`src/emit.rs`**: a kernel-built histogram's `sum` from `total_ns`, not the
  buckets' low bounds ("a kernel-built histogram's `sum`").
- **`src/resolve.rs`**: the chain's order for a backend both unbuilt and
  unservable, on the forced path too; `scenario_net`'s `net.hwts` test then
  accepts one exit code (file that request to its owner, do not edit it).
- **`tests/cross_memory.rs`**: `cross-memory-which-allocation.yml` meets a
  user-mode-only run on `defaults`; at paranoid 2 it skips with *"this machine
  withholds a source it needs: caveat: ..."*. Branch on the result's
  *"page faults in user mode only"* note and assert what the user-mode half
  can, as `scenario_mem::sampled_in_user_mode_only` does.
- **`docs/portability.md:184`**: `sched.switches` is usable on 5.15 through
  `pmu:sw`.
- **The GPL-2.0 text**, as decided above, and the README's licence lines.

### python -- an interpreter and a server (after symbols and cases)

Case 3, **CPython 3.14, a pure-Python hot loop**: the CPU cause one function
dominating by construction of the input; the off-CPU cause a blocking read.
Reference `cProfile`. With `-X perf` the verdict names the Python function
(symbols); without it, it must say "interpreter frames", never a C function as
the suspect. Case 4, **`python3 -m http.server`** under the test's own client
(no load generator is installed; the client is also the latency clock): a
CPU-bound handler path (a large directory listing) and a served file on a cold
cache (`posix_fadvise(DONTNEED)` from the harness). Reference: the client's
clock and the server's `/proc/<pid>/schedstat`.

### build -- durability, a compiler and a build (after cases)

Case 2, **`sqlite3` durability**: `journal_mode=DELETE`, `synchronous=FULL`
against WAL; reference the commit count times the fsyncs per commit and
`/proc/<pid>/io`. Case 5, **`gcc -O2`** on a known translation unit: `-O2`
against `-O0` (two builds, the difference is the cause), headers on a cold
cache; reference `-ftime-report` and the process tree gcc -> cc1 -> as. Case 6,
**`make -j` on the sqlite amalgamation's build**: the critical path, one long
serial job, and many short processes; reference `make --trace` timings and
per-process `schedstat`.

### io -- the in-kernel histogram costs 23.6 % at 3 M IOPS

Found by the coordinator before this wave, running the privileged list:
`scenario_io::the_in_kernel_histogram_costs_a_two_million_iops_load_under_one_percent`
on `null_blk` (the doc comment's commands, granted by the sudoers file) reads
*"bare 3158519 IOPS, under the in-kernel histogram 2413950: 23.57 %"* against
phase 6's 1 %. The bench agrees in kind: `io.latency` through `bpf:raw_tp` at
3.44 % of the target's cpu against `low`'s 1 %, `io.size` 6.45 %, `io.queue`
2.85 %. Find where the cost is -- the programs' own run time
(`sysctl kernel.bpf_stats_enabled`, granted, is machine wide: ask the
coordinator), the map updates per request, a per-request lookup that a
per-cpu array would avoid, the issue/complete pairing, or the attach points --
measure it, and bring it under the criterion or say, with numbers, why the
criterion is the wrong one. The test needs `null_blk`, which is machine wide:
**ask the coordinator to run it** with your branch, alone, rather than loading
the module while the other streams build. You may use
`sudo -n /usr/bin/modprobe null_blk ...` yourself only for a functional check
at low load.

---

## Exit criteria for the wave

- The six cases each have two known causes, one on the CPU and one off it, and
  the triage's first suspect is the known cause for the objective asked --
  or the case is filed as a finding against what got it wrong, with the case
  that shows it.
- Both ways of asking name the same suspect.
- Every case runs at `perf_event_paranoid` 2 in the CI's `defaults` cell or
  skips there with the machine's reason first.
- The in-kernel histogram under 1 % at 2 M IOPS, or the criterion argued.
- The engine and core filings landed or re-filed with what blocked them.
- `make check` green at the merge.
