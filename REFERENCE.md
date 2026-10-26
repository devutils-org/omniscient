# Omniscient — reference

[Back to the overview](README.md). Detailed usage and implementation notes.

`omn` measures a Linux program or machine straight from the kernel --
`perf_event_open`, eBPF, procfs, sysfs, netlink; no `perf`, `bpftrace` or
`bcc` underneath -- and every number it prints says what it lost, how it was
obtained, and how far it can be trusted. A request that cannot mean anything
is refused with the reason instead of answered with a number that means
something else.

## Install

From a release archive (x86_64, glibc 2.17 or later; Linux 5.15 is served, and
each backend states the kernel it needs, `docs/install.md`):

```console
$ sha256sum --check omn-0.1.0-x86_64-linux-gnu.tar.gz.sha256
$ tar -xzf omn-0.1.0-x86_64-linux-gnu.tar.gz -C ~/.local --strip-components=1
$ sudo setcap cap_bpf,cap_perfmon+ep ~/.local/bin/omn
```

Or from source, with Rust 1.82 or later:

```console
$ make install PREFIX=~/.local
$ sudo setcap cap_bpf,cap_perfmon+ep ~/.local/bin/omn
```

The two capabilities are what its eBPF sources need; without them `omn` still
runs and serves fewer sources, and says which ones and why. `docs/install.md`
has what each distribution needs and what `perf_event_paranoid` changes.

`omn` runs no other tool. The comparison with the tools it replaces
(`docs/replaces.md`) does, as witnesses in `tests/reference.rs`, and each
pair whose tool is missing skips with the reason. To run all of them, a
development machine needs `perf`, `strace`, `bpftrace`, the bcc tools
(libbpf-tools' `memleak`, `offcputime`, `runqlat` and the rest), sysstat
(`iostat`, `mpstat`, `pidstat`, `sar`), `numactl`, procps (`vmstat`, `top`,
`pmap`, `slabtop`), `smem`, valgrind, heaptrack, iproute2 (`ip`, `ss`,
`nstat`), `tcpdump`, `ethtool`, `turbostat` and `cpupower`, `lm_sensors`,
`powertop`, `fio`, and for the GPU pairs `nvidia-smi`, `nsys` and `ncu`.
Some need more than installing: `bpftrace`, `memleak` and `tcpdump` need
root (`sudo -n`, or the test run as root), `perf trace`, `perf sched` and
`perf lock` a readable `/sys/kernel/tracing`, `net.capture` `CAP_NET_RAW`
(`make CAPTURE=1`), the RAPL files and `cpupower monitor -m Mperf` root or a
grant, and the cpu-wide `perf` and `turbostat --no-msr` pairs
`perf_event_paranoid` at most 0. `catalog/references/README.md` says how a
pair runs and skips; `docs/dev/STATE.md`, "What remains", lists the exact commands.

## Try it

```console
$ omn list | tail -n 3 | head -n 1
101 sources: 92 usable here, 0 unverified, 9 out of reach on this machine.

$ omn mem.rss --emit timeline --mode poll@100ms -- ./build.sh
mem.rss  via fs:proc:smaps_rollup  (mem.rss)
  11 events over 1.10s, stopped because the launched program exited with exit status 0
  no loss: every read succeeded and no tick ran late

  timeline
    task:170997
      +0ns        12.0 KiB
      +100ms      17.6 MiB
      +300ms      25.6 MiB
      +901ms      49.6 MiB
```

`omn list` is what this machine can serve, probed now; `omn list <source>`
says why a source is out of reach. `man omn` is the manual, `docs/DEMO.md` is the
tool in one page, and `docs/GUIDE.md` walks through more than a hundred and fifty
sessions in `examples/`.

---

## How it works

A measurement is described along five axes -- **source**, **target**,
**mode**, **emit**, **keys** -- and the combinations that cannot mean anything
are refused rather than answered. Because it does not wrap another tool, there
is no second output format to parse and no second clock in the middle of the
pipeline.

## The number no single source can give

`malloc()` allocates nothing: it returns an address, and the pages become
resident on first write. So an allocation-site profile and a memory curve never
line up, and neither answers *which allocation actually cost memory*.

The shim reports the range each allocation owns; every page fault carries the
address that faulted; a fault inside a range is that allocation becoming real.

```console
$ omn -f examples/allocation-cost.yml
join pages-of-allocation  mem.heap.alloc -> mem.faults  evidence KEY
  2144 of 3012 `mem.faults` events attributed (71.2 %); 868 fell outside every open range
  states: 2556 opened, 2525 closed, 31 still open at the end, peak 1580 held at once
  no eviction: nothing was dropped for age or for space

  heaviest ranges
    RANGE                                    SIZE    EVENTS  OUTCOME
    mem:[0x7f51651fb010,0x7f51659fb011)    8.0 MiB       515  freed
    mem:[0x7f51659fc010,0x7f51661fc011)    8.0 MiB       515  freed
```

`evidence KEY` is the point. A correlation backed by a propagated identity and
one backed by two things happening at the same time are both "correlations", and
only one of them is evidence. Every joined result says which it is:
`IDENTITY` > `KEY` > `WAKEUP` > `TEMPORAL`.

## What it refuses

```console
$ omn mem.bandwidth --pid 1 --mode count --emit rate
error: `mem.bandwidth` cannot be measured per pid: the counter is scoped to the
       package, so no per-process number exists to return
  hint: this source accepts system

$ omn io.latency --system -m trace -e hist --filter "bites > 4096"
error: `io.latency` has no field `bites` to filter on; did you mean `bytes`?
  hint: `io.latency` exposes dev, rwbs, bytes, comm
```

Returning a machine-wide value while presenting it as per-process attribution is
the worst bug this tool could have. So the validator rejects, before anything is
attached: a target a source cannot carry, a mode it does not support, an emit
that mode cannot produce, a key it does not carry, a filter field it does not
expose, an output format that cannot express the emit.

The same applies to privileges. An unprivileged run is refused with the
capability that is missing, never resolved onto a lesser backend that answers a
different question:

```console
$ omn sched.wakeup --mode trace --emit raw -- ./server
error: `sched.wakeup` has no viable backend on this machine:
  bpf:raw_tp: loading a tracing program needs CAP_BPF and CAP_PERFMON.
    Grant them with `sudo setcap cap_bpf,cap_perfmon+ep $(which omn)`.
```

## What this machine can serve

```console
$ omn list mem
SOURCE             MODES                  COST     DET     HERE  COLLECTOR  SUMMARY
mem.rss            poll, snapshot         free     medium  yes   all        Resident footprint (pages in RAM, shared ones counted in full)
mem.heap.retained  poll, snapshot         extreme  high    yes   all        Fragmentation: memory retained by the allocator but no longer live
mem.bandwidth      count, poll            free     low     no    none       DRAM throughput
mem.reclaim        trace, count           low      medium  yes   1 of 2     Direct reclaim
…
23 sources: 22 usable here, 0 unverified, 1 out of reach on this machine.
22 of them have a collector, covering 29 of their 45 backends. …
```

Three different questions, kept apart. **HERE** is what this machine offers,
probed now — not inferred from the kernel version or the cpu vendor. **`?`** is
not a soft yes: it means no probe covers that requirement, so nothing was
checked, and the resolver will not select such a backend while a verified one
exists. **COLLECTOR** is what this build can measure, which is a different
question again: a source can be reachable here with nothing written for it.

`docs/CATALOG.md` is the vocabulary and `catalog/*.yaml` is the same thing as data.
Adding a source means adding an entry: it appears in `omn list`, in validation
and in the help without touching code.

## Trace as code

A session file describes exactly what a command line does, and
`--dump-request` prints one, so a measurement is prototyped in the shell and
frozen into a file without being retyped.

```yaml
name: which allocation actually cost memory
duration: 30s

requests:
  - source: mem.heap.alloc
    target: {launch: [./server]}
    mode: trace
    emit: [summary]
  - source: mem.faults
    target: {launch: [./server]}
    mode: trace
    emit: [rate]

joins:
  - id: pages-of-allocation
    from: mem.heap.alloc
    to: mem.faults
    via: mem
    match: interval
    opens: "op == 'alloc'"
    closes: "op == 'free'"
    window: 60s
```

YAML, JSON and TOML are all accepted. `docs/GUIDE.md` explains the sessions in
`examples/` and what each one shows; `examples/README.md` is the
index; `scripts/demo.sh <family>` runs one family's and `make demo-all` runs the
lot.

## The record beside the numbers

```console
$ omn proc.fd --pid 4242 -d 30s -e summary --out ./results
$ ls results
cost.json  resolved.json  results.txt
```

`resolved.json` holds the backend that was selected, every requirement and how
it was probed, what was degraded, and the machine's kernel, governor, THP mode,
SMT state and clocksource. Two measurements taken through different backends are
not comparable, and without that file nobody can tell that they were.
`cost.json` holds what the measurement itself cost, in cpu, against the target's
own — because an observation that changed the thing observed is a different
kind of incomparable.

## Output formats

`text` for a terminal, `json` / `yaml` / `toml` for tooling and for the resolved
file, `ndjson` for a record stream, and the profile and trace formats
`folded`, `speedscope`, `pprof` and `chrome`. All nine are written. Each is
declared with the emits it can carry and accepts only those:
`--emit hist --format folded` is refused, not guessed at. `omn formats` lists
them.

## Boundaries

Stated up front, because a project called "Omniscient" that hides them turns
them into perceived bugs.

- **No bit-exact determinism.** Counters are stable — `cpu.instructions` has a
  run-to-run spread under 0.1 % here — and never exact: skid, speculation, IRQs,
  SMT.
- **No inside view of NVIDIA GPUs.** CUPTI is closed and there is no kernel
  path; AMD and Intel have the full stack, and the catalog reflects the
  asymmetry rather than hiding it.
- **Writes are less attributable than reads.** Writeback leaves in a kworker and
  the originating context is gone: `KEY` evidence, not `IDENTITY`.
- **Single machine.** Inter-machine clock drift is a different class of problem.
- **It observes; it does not tune.** No `madvise`, no `sched_setaffinity`, no
  queueing discipline, no cgroup limit — and nothing that modifies the target in
  order to read it, which is why `ptrace` and `userfaultfd` are not backends
  here. A tool that measures and changes in one run produces numbers nobody can
  interpret afterwards. It will tell you readahead is doing nothing for this
  workload; what to do about that is yours.
- **LTTng and its kind are peers, not backends.** They reach the same kernel
  tracepoints by another path, so wrapping one would mean shelling out — see the
  first boundary. If LTTng-UST ever earns a place it is as a CTF import: one
  more format read, not one more source.

## State

`docs/dev/STATE.md` has the measured numbers, where each roadmap phase stands, and what
remains in the order it is worth doing. In short: the skeleton is finished and
96 of 101 sources collect for real, over eight mechanisms: procfs and sysfs,
`perf_event_open` counting and sampling, netlink, an `LD_PRELOAD` shim, NVML
through `dlopen`, `af_packet` with a `PACKET_MMAP` ring, and eBPF through raw
tracepoints, classic tracepoints, kprobes, uprobes, fentry and USDT. All four
join match kinds have a producer, and a decomposition says where a task's wall
clock went with a residue inside its own resolution.

Above the engine, since the tenth and eleventh waves: a request can be attached
and detached inside a run; `omn latency`, `omn saturate` and `omn diff` name
what limited a run for an objective the user states; and a session's
`escalate:` block lets the run pay for one detailed layer at a time, the one
its cheap pass names. The tool has been booted under four kernels and five
configurations (`docs/portability.md`), and what it found there is the current
wave's work.

1830 tests pass, about 360 of them scenarios: a workload whose property the
scenario chose, a session that measures it, and a check against that property.
They found 63 defects in the fourth wave alone, none worked around, and
`docs/scenarios/` records every one.

## Documentation

| | |
|---|---|
| `docs/install.md` | installing, the two capabilities, what a stock distribution serves |
| `docs/how-to-use/` | per family: which source for which question, and how to choose its target, mode, cost and backend |
| `man/omn.1` | the manual page — `make man` |
| `docs/CATALOG.md` | the vocabulary: axes, backends, families, caveats |
| `docs/dev/STATE.md` | what works today, and what is left |
| `docs/ROADMAP.md` | the milestones and their exit criteria |
| `docs/DECISIONS.md` | what is settled, what is open, and why |
| `docs/contributing.md` | how to add a source |
| `docs/open-findings.md` | findings that cut across families |
| `docs/families/*.md` | what each family's measurements taught |
| `docs/bpf-layer.md` | how a program is loaded and attached |
| `docs/GUIDE.md` | how to run the demonstrations, and what each session shows |
| `docs/triage.md` | the intelligence layer: objectives, verdicts, the escalation |
| `docs/bench.md` | what each source costs, measured on this machine |
| `docs/portability.md` | the kernel matrix, the degradation runs, the CI |

## Building, for development

```console
make          # build, and grant the capabilities a relink drops
make check    # test, clippy and fmt, the way CI would
make demo     # the guided tour: eight questions, against this machine
make dist     # the release archive and its checksum, scripts/release.sh
```

`make` rather than `cargo build`: the preload shim is a workspace member nothing
depends on, and the `CAP_BPF` and `CAP_PERFMON` file capabilities do not survive
a relink. Both failures are silent — the tool simply reports fewer sources as
available, which reads like the machine changed rather than the binary.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.

The code under `src/bpf/`, which emits the programs loaded into the kernel, is
licensed under either of the MIT license or the GNU General Public License,
version 2 (`MIT OR GPL-2.0`), and the programs declare `Dual MIT/GPL` to the
kernel: several helpers they call are available to GPL-compatible programs only.
The GPL-2.0 text is in [`LICENSES/GPL-2.0-only.txt`](LICENSES/GPL-2.0-only.txt),
and `make install` and the release archive ship it under
`share/doc/omn/LICENSES/`.
