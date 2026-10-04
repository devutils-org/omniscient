# Omniscient — Source catalog

The tool's single vocabulary. This file is **the data**, not documentation: the
CLI, the help output, `omn list` and the request validator are all projections of
it. Adding a source means adding an entry here, and it shows up everywhere
automatically.

> ⚠️ **Nothing below may be hard-coded as available.**
> Tracepoint names, PMUs and sysfs paths vary with kernel, architecture, vendor
> and privileges. Every entry declares an **ordered backend chain**; the resolver
> probes actual availability at startup (`/sys/kernel/tracing/events`,
> `/sys/bus/event_source/devices`, kernel BTF) and keeps the first viable
> backend. The selected backend is **written into the resolved file**: two
> measurements taken through different backends are not comparable unless you
> know it.

---

## 0. Conventions

**Request axes**

```
source   <family>.<metric>          io.latency, cpu.cycles
target   what to observe            pid | tid | cgroup | cpu | system | ./prog
mode     how to observe             count | sample | trace | poll | snapshot
emit     what comes out             raw | hist | rate | topk | stack | timeline
keys     correlation keys           req | task | file | sock | blk | mem | gpu
filter   predicate                  pushed into the kernel when possible, else userspace
```

**Backend notation**

| Tag | Mechanism |
|---|---|
| `pmu` | `perf_event_open`, hardware counter or sampling |
| `pmu:sw` | `perf_event_open`, software event (portable) |
| `pmu:aux` | `perf_event_open` + AUX buffer (Intel PT, CoreSight) |
| `bpf:fentry` | eBPF on fentry/fexit (BTF required, cheapest) |
| `bpf:kprobe` | eBPF on kprobe (fallback without BTF) |
| `bpf:tp` | eBPF on a stable tracepoint |
| `bpf:uprobe` | eBPF in userspace (expensive, ~1-2 µs) |
| `bpf:usdt` | userspace static probe (if the binary carries one) |
| `preload` | `LD_PRELOAD` shim (~10-50 ns, prefer over `uprobe`) |
| `fs:*` | file reads: `proc`, `sys`, `cgroup`, `tracefs` |
| `netlink` | `sock_diag`, `taskstats`, `ethtool` |
| `af_packet` | TPACKETv3 capture |
| `vendor:*` | closed SDK: `cupti`, `rocprof`, `l0`, `nvml` |

**Cost scale**

| Tier | Order of magnitude |
|---|---|
| `free` | counter read, ~0 |
| `low` | < 1 % |
| `med` | 1-5 % |
| `high` | 5-20 % |
| `extreme` | > 20 %, with no bound above (a collector running inside the target: the preload shim, `proc.syscall`'s program; kernel replay) |

A family table's Cost column lists the tiers its chain declares, in chain
order and without repeats (`low / med`: the first backend is `low`, a fallback
`med`); `catalog/*.yaml` holds each backend's, measured by the bench where a
row has been measured.

Cost is **throughput-dependent**: a tracepoint at 1 kHz is `low`, the same one at
2 MHz is `high`. The resolver must be able to switch `trace` → in-kernel `hist`,
or `trace` → `sample`, when the observed rate exceeds the budget.

**Evidence levels** (carried by every event and every span)

`IDENTITY` (propagated key) > `KEY` (shared typed key) > `WAKEUP` (wakeup chain)
> `TEMPORAL` (overlap only, a hypothesis).

A session's join rule names the key it matches on with `via`. `via` is
optional for `overlap` alone: without it, the `from` source is read as one
series, keyed `machine`, and every `to` event is laid across its intervals. A
keyless rule of any other kind is refused before the run, and a keyless
`overlap` whose `from` source carries several series (a per-device or per-cpu
poll) is refused by name.

---

## 1. Catalog entry schema

```yaml
id: io.latency
family: io
summary: "Block request latency, submit → completion"
unit: ns
primitive: true                  # false = derived from other sources
backends:                        # ordered, first viable one wins
  - id: bpf:tp
    requires: [tracepoint:block:block_rq_issue, tracepoint:block:block_rq_complete, bpf]
    join: {key: "blk:rq@open_ts", scope: scoped_eq, ttl: 30s}
    cost: low
  - id: fs:tracefs
    requires: [tracefs]
    cost: med
    degraded: "no per-task attribution, no pushed-down filter"
  - id: fs:proc
    requires: [/proc/diskstats]
    cost: free
    targets: {system: native}    # only where the chain disagrees
    filter_fields: [dev]         # only where the chain disagrees
    degraded: "aggregated per device, no per-request latency"
modes:  [trace, count]
emits:  [hist, raw, rate, topk]
targets:
  system: native
  cgroup: native                 # io.stat / bpf filter
  pid:    bpf_only               # needs the submission context
  cpu:    no
keys:   [blk, task, file, req]
filter_fields: [dev, rwbs, bytes, comm]
determinism: low                 # a timing metric
caveats:
  - "Reads are attributable to the submitter; writes are not (writeback → kworker)."
  - "Request pointers get reused: key = (ptr, open_ts)."
```

`targets`: `native` (filtered at the backend level), `bpf_only` (filtered inside
the program), `no` (invalid combination, must fail validation).

A backend may carry its own `targets` and `filter_fields`. Absent, it serves
and carries whatever the entry declares, which is the common case. Present, the
map speaks only for that backend and is not widened by the entry's: a target
kind it does not mention is one it cannot serve, and a field it does not list is
one its events never carry. The entry describes the union of what the chain can
do, and a backend that only attributes per cgroup must not be handed a
system-wide request because a sibling could serve one; a predicate over a field
the selected backend never emits would empty the series rather than narrow it.
The loader refuses a backend that claims a target or a field the entry does not
offer, and an entry that offers one no backend in its chain serves.

A backend may also declare `unit:` when its values are in another unit than the
entry's: `cpu.cycles` through `pmu:sw` reads the cpu clock, so it says
`unit: ns` where the entry says `count`, and a result served by it is in
nanoseconds (`Entry::backend_unit`). The loader refuses it on the head of the
chain, on a backend declared `equivalent`, where it repeats the entry's, and
where it does not add up while the entry's does (`check_backend_units`).

`requires` lists what the backend needs, each string answered by
`Capabilities::probe` in `src/capabilities.rs` as available, missing or unknown.
The classes it answers include a path (`/proc/diskstats`, with `[pid]`
substituted), `kernel>=<version>`, `tracepoint:<sys>:<event>` (the event's `id`
file must be readable, not only its directory), `pmu:<event>`, `kprobe`, and
**`fentry:<function>`**: the running kernel's BTF declares a `FUNC` of that name
and, where this process may load a tracing program, the smallest one loads
against it (`src/bpf/fentry.rs`). Since the fourteenth wave it may carry the
prototypes the program reads, spelled and separated as a raw tracepoint's
below -- `fentry:tcp_v4_connect(sock *, sockaddr_unsized *, int)|(sock *,
sockaddr *, int)` -- so a kernel with a shape the program has never seen reads
*missing* instead of failing at the attach; a bare name asks for existence
only. Four more say what a program reads, not only
where it attaches, because the portability matrix found sources that probed
available on an older kernel and then refused at the attach:

- `raw_tracepoint:<event>` asks the kernel's BTF whether the event exists.
  `raw_tracepoint:<event>(<arg>, ...)` also asks for the prototype the program
  reads, spelled as `Btf::tracepoint_signature` spells it (no `struct`, no
  qualifier): `raw_tracepoint:block_rq_issue(request *)`. A program that reads
  more than one shape lists each, `name(a, b)|(a, b, c)`. The program's
  constant and this string change in the same commit.
- `btf_member:<struct>.<member>` is a member a program reads, looked up the way
  the program looks it up: `btf_member:sock.sk_ino`.
- `uprobe_multi` means a `BPF_TRACE_UPROBE_MULTI` link can be made (Linux 6.6),
  asked of the kernel. `uprobe` means *this process can place a uprobe*: that
  link, or the `uprobe` PMU with `CAP_SYS_ADMIN`.
- `cgroup2:<file>` is a cgroup file readable where this process lives, and
  `cgroup2:<file>:<key>` a flat-keyed one that carries that key.

Two more since the thirteenth wave. `writable:<path>`
(with `[pid]` substituted) is a file this process may write, asked with
`access(W_OK)` -- `mem.wss` writes `clear_refs`, and a file that reads fine can
still refuse the write. And a qualified `pmu:<unit>/<event>/` answers
*missing*, naming the unit or the event, when the machine does not publish it,
not unknown: a runner without RAPL listed `power.energy` as assumed and refused
it at the start.

A string no probe answers -- `usdt:libc` today -- reads unknown, and unknown
never counts as available. A requirement a collector's own start decides is
probed by that start: `phc` by `hwts::clock_support`, `ethtool` by resolving
the netlink family. A counter the kernel raises from its own code (a context
switch, a migration) is probed with the kernel included, and a machine that
would open it for user mode only answers missing: that count is zero whatever
the program does.

**Usable means a backend this build collects.** `omn list` calls a source
usable here only through a backend with a collector; one whose only viable
backends are unwritten is out of reach, with each backend's reason below it.
The resolver's order does not change: a request still resolves to an unwritten
backend when no written one is viable, and is refused as the build's gap.

Two fields a backend may carry beside `cost`, and each is a statement about what
its events mean rather than about how it is reached:

- `equivalent: true` — this fallback loses nothing against the backend above it.
  A backend that is not first either says this or says what it `degraded`s;
  saying neither is refused when the catalog loads.
- `drained: true` — one event stands for many occurrences, added up in the
  kernel and read on a tick. It is what lets the validator refuse a `wakeup`
  join onto such a source *before* the run: a wakeup releases one block, and an
  event that is a sum of blocks cannot be the one it released. An entry is
  `always_drained` only when every backend of its chain is, so a partial flag
  never refuses a chain that still has a per-occurrence backend.

### Unwind modes

A stack is unwound by one of several mechanisms, and they are declared, probed
and chosen the way backends are: ordered best first in `catalog/unwind.yaml`
(compiled in; it moves to `catalog/` once the loader skips it), each with its
`requires`, the bytes of user stack the kernel `copies` per sample, a `cost`,
and what it `degraded`s against the modes above it. The loader holds the table
to the fallback rule, and refuses a mode declared twice or a declared `auto`.

| Mode | Requires | Copies | Cost | Degraded | Served |
|---|---|---|---|---|---|
| `sframe` | `kernel:sframe`, `target:sframe`, `perf:callchain` | 0 | low | (first) | no: no kernel through 7.2 reads `.sframe` |
| `fp+leaf` | `perf:callchain`, `perf:user_stack`, `target:frame_pointers`, `target:cfi` | 256 | low | only the leaf's caller is recovered, and marked ` [recovered]` | yes |
| `dwarf` | `perf:user_stack`, `target:cfi` | 8192 | high | 8 KiB per sample; a deeper stack is cut where the copy ends, counted | yes (`framehop`) |
| `lbr` | `pmu:branch_call_stack` | 0 | low | 16 to 32 calls | no: probed, and no collector decodes branch stacks |
| `fp` | `perf:callchain` | 0 | low | the caller of a frameless function is lost | yes |
| `none` | -- | 0 | free | no stack; never chosen by `auto` | yes |

`--stack auto`, the default, takes the first mode whose every requirement is
available, and unknown never counts as available. The `target:` requirements
are read from the binary a launch names or `/proc/<pid>/exe`, so the choice is
the target's as well as the machine's: `fp+leaf` for a build with frame
pointers, `dwarf` for one without, `fp` for a system-wide or cgroup profile,
whose binaries are not known before the run. `--stack <mode>` forces one:
refused before the launch where a requirement is missing, served with
`assumed rather than checked` where one is only unknown. The profile says which
mode unwound it, what `auto` passed over and why, and at a paranoia that
refuses the kernel, that every mode -- `none` included -- is user-only.

`--symbols local`, the default, names a frame from the files the target
mapped (their `.symtab` or `.dynsym`), `/proc/kallsyms` for the kernel half,
and, for an address in an anonymous executable mapping no file names -- a
JIT's code, an interpreter's trampolines -- the `/tmp/perf-<pid>.map` the
target itself wrote, re-read as it grows. That map is a file on this machine,
so it needs no rung of its own; a frame it named prints `py::<function>:<file>`
for CPython's map, and the facts line's `names came from --` carries
`perf-<pid>.map: perf-map, the <n> lines the program wrote for code no file holds`.
`--symbols debuginfod` reads all of that first, then names a stripped file's
frames from `DEBUGINFOD_URLS` by its build id, through elfutils' own cache, and
never unless asked. Each frame says which of them named it (`elf`,
`kallsyms`, `perf-map`, `debuginfod`).

---

## 2. Family `cpu`

| Metric | What it measures | Backends | Modes | Targets | Cost | Det. |
|---|---|---|---|---|---|---|
| `cpu.cycles` | unhalted cycles | `pmu` (HW_CPU_CYCLES) | count, sample | all | low | medium |
| `cpu.instructions` | retired instructions | `pmu` (HW_INSTRUCTIONS) | count, sample | all | low | **high** |
| `cpu.ipc` | derived cycles/instr | — | count | all | low | medium |
| `cpu.branch_misses` | mispredicted branches | `pmu` | count, sample | all | low | medium |
| `cpu.cache_misses` | LLC misses | `pmu` (HW_CACHE_MISSES) | count, sample | all | low | low |
| `cpu.topdown` | TMA: front-end / back-end / bad-spec / retiring | `pmu` (slots + topdown-*) | count | all | low | medium |
| `cpu.profile` | on-CPU profile with stacks | `pmu:sw` (CPU_CLOCK) or `pmu` cycles + callchain | sample@freq | all | low / med | low |
| `cpu.oncpu_time` | effective CPU time | `bpf:raw_tp` sched_stat_runtime (poll, per thread) · `bpf:tp` sched_switch · `fs:cgroup` cpu.stat · `fs:proc` | trace, poll | pid, tid, cgroup, system | low / free | low |
| `cpu.context_switches` | context switches | `pmu:sw` | count, sample | pid, cgroup, cpu | low | low |
| `cpu.migrations` | cross-core migrations | `pmu:sw` (CPU_MIGRATIONS) | count | pid, cgroup | low | low |
| `cpu.util` | utilization by state | `fs:proc` /proc/stat · `fs:cgroup` cpu.stat | poll | system, cpu, cgroup | low / free | low |
| `cpu.pressure` | PSI CPU stall | `fs:proc` /proc/pressure/cpu · cgroup cpu.pressure | poll | system, cgroup | free | low |
| `cpu.throttle` | CFS cgroup throttling | `fs:cgroup` cpu.stat (nr_throttled) | poll | cgroup | free | medium |
| `cpu.flow` | exhaustive control flow | `pmu:aux` intel_pt / cs_etm | trace | pid, cpu | high | high |

**Caveats**

- `cpu.instructions` is the most stable metric of the lot (typically CV < 0.1 %)
  but **never bit-exact**: interrupt skid, speculation, IRQs, SMT. Strict
  determinism belongs to DBI (cachegrind), outside the kernel-level core.
- `cpu.topdown` is **heavily microarchitecture-dependent**. The catalog must map
  it per CPU family rather than hard-code event names.
- `cpu.profile` with stacks: the unwind mode is chosen by `--stack auto` or
  forced (§1, "Unwind modes"), and the profile says which. `dwarf` copies 8 KiB
  of stack on every sample → about twice the tool's cpu per sample of `fp`,
  measured, and rings that fill a hundred times faster.
- `cpu.flow`: the kernel provides raw bytes, decoding is a separate concern
  (libipt + binary images + sideband). A late-phase deliverable.
- `cpu.oncpu_time` through `bpf:raw_tp` adds up each thread's increments of
  the scheduler's own `sum_exec_runtime` (`sched_stat_runtime`) in the kernel
  and drains them every poll: `poll` only, one series per thread
  (`task:<tid>`, its process as `pid`); summing the threads gives the
  process. `trace`, a slice per event, stays on `bpf:tp` until a view asks
  for it. `cpu.oncpu_time` emits `raw`, so a view keeps each poll's figure
  to the end of the run.
- Disabling SMT or pinning affinity shifts counters by 10-30 %: capture it in the
  resolved environment.

---

## 3. Family `mem`

| Metric | What it measures | Backends | Modes | Targets | Cost | Det. |
|---|---|---|---|---|---|---|
| `mem.rss` / `.pss` / `.uss` | resident footprint | `fs:proc` smaps_rollup → smaps | poll, snapshot | pid, cgroup | med / free | medium |
| `mem.cgroup` | cgroup accounting | `fs:cgroup` memory.current / .peak / .stat | poll | cgroup | free | medium |
| `mem.faults` | minor/major page faults | `pmu:sw` PAGE_FAULTS_MIN/MAJ · `bpf:tp` | count, sample, trace | all | low | medium |
| `mem.page_alloc` | kernel page allocations | `bpf:tp` kmem:mm_page_alloc / mm_page_free | trace | system, cgroup | med | medium |
| `mem.kmalloc` | slab allocations | `bpf:tp` kmem:kmalloc / kfree | trace | system | med | medium |
| `mem.heap.alloc` | userspace allocations by site | `preload` → `bpf:uprobe` malloc/free · `bpf:usdt` | trace | pid | extreme | high |
| `mem.heap.live` | live heap | `preload` (delta) · jemalloc/tcmalloc stats | poll | pid | extreme / free | high |
| `mem.heap.retained` | **fragmentation**: retained / live | derived (`heap.live` vs `rss`) | poll | pid | extreme | high |
| `mem.cache_l1` / `.llc` / `.tlb` | misses per level | `pmu` (L1-dcache-load-misses, LLC-*, dTLB-*) | count, sample | all | low | low |
| `mem.latency` | load latency | `pmu` PEBS load-latency · AMD IBS | sample | all | med | low |
| `mem.sharing` | **false sharing** (HITM) | `pmu` PEBS HITM · IBS | sample | all | med | low |
| `mem.bandwidth` | DRAM throughput | `pmu` uncore IMC · Intel RDT/MBM | count | **system only** (MBM: cgroup) | free | low |
| `mem.numa` | access locality | `pmu` PEBS data-source · `fs:proc` numa_maps | sample, poll | all | med | low |
| `mem.wss` | real working set | `fs:sys` DAMON · page_idle | poll | pid, cgroup, system | low / high | medium |
| `mem.reclaim` | direct reclaim | `bpf:tp` vmscan:mm_vmscan_direct_reclaim_* | trace | system, cgroup | low | medium |
| `mem.compaction` / `.thp` | defrag, huge pages | `bpf:tp` compaction:* · `fs:sys` | trace, poll | system | low | low |
| `mem.swap` | swap in/out | `fs:proc` vmstat · `fs:cgroup` memory.swap.current | poll | system, cgroup | free | low |
| `mem.pressure` | PSI memory | `fs:proc` /proc/pressure/memory | poll | system, cgroup | free | low |

**Caveats**

- **State the accounting metric explicitly.** RSS, PSS, USS, live heap, retained
  heap and `memory.current` are six different numbers, all correct. A `mem`
  request without a named metric must fail rather than pick a default.
- `malloc()` allocates nothing: RSS only rises on first write. An allocation-site
  profile and an RSS curve **never line up**. Correlate `mem.heap.alloc` with
  `mem.faults` through the `mem:[base,len)` key (interval join) to attribute
  memory to whoever brings it into existence.
- `free()` returns nothing: the allocator keeps its arenas. `mem.heap.retained`
  is the metric that predicts OOMs in production, and almost nobody collects it.
- **THP breaks reproducibility**: `khugepaged` promotes in the background, RSS
  grows in 2 MB steps. Freeze and log the mode
  (`/sys/kernel/mm/transparent_hugepage/enabled`) just like clock frequencies.
- Uncore `mem.bandwidth` is **inherently system-wide**: refuse `--pid`.
  Per-process attribution needs RDT/MBM, available only on some CPUs.

---

## 4. Family `io` (block + filesystem)

| Metric | What it measures | Backends | Modes | Targets | Cost | Det. |
|---|---|---|---|---|---|---|
| `io.latency` | submit → completion (device) | `bpf:tp` block_rq_issue/complete · `fs:tracefs` · `fs:proc` diskstats | trace, count | system, cgroup, pid* | med / free | low |
| `io.queue` | insert → issue (software wait) | `bpf:tp` block_rq_insert/issue | trace | system, cgroup | med | low |
| `io.bytes` / `.iops` | volume and throughput | `bpf:tp` · `fs:proc` diskstats · `fs:cgroup` io.stat | count, trace, poll | system, cgroup | low / free | low |
| `io.size` | request size distribution | `bpf:tp` block_rq_issue | trace | system, cgroup | high | medium |
| `io.merges` | request merges | `bpf:tp` block_rq_merge · diskstats | count | system | low / free | medium |
| `io.inflight` | queue depth | `fs:proc` diskstats | poll | system | low | low |
| `io.syscall` | latency as the application sees it | `bpf:tp` raw_syscalls · `bpf:fentry` vfs_read/write | trace | pid, cgroup | med | low |
| `io.fs` | VFS/FS latency | `bpf:fentry` vfs_* · ext4/xfs/btrfs tracepoints | trace | pid, cgroup | high | low |
| `io.pagecache` | page cache hit/miss | `bpf:tp` filemap:mm_filemap_add_to_page_cache + faults | trace | pid, system | med | low |
| `io.readahead` | readahead effectiveness | `bpf:tp` readahead tracepoints | trace | system | low | low |
| `io.writeback` | deferred writeback | `bpf:tp` writeback:* | trace | system, cgroup | low | low |
| `io.fsync` | sync latency | `bpf:fentry` vfs_fsync · ext4_sync_file | trace | pid | high | low |
| `io.pressure` | PSI I/O | `fs:proc` /proc/pressure/io | poll | system, cgroup | low / free | low |

**Caveats**

- `io.latency` with `--pid` is reliable **for reads only**: reads are submitted in
  the reader's context, so `bpf_task_storage` is readable at `block_rq_issue`
  (`IDENTITY` evidence). Writes leave through writeback in a kworker: the context
  is lost, falling back to `file:dev+ino` (`KEY` evidence).
- **Three layers not to be conflated**: syscall latency (what the application
  suffers), block latency (what the device does), FS latency (journal, locks,
  allocation). The gap between them *is* the diagnosis.
- Join key: `(request pointer, open timestamp)`. Pointers get recycled — plain
  equality produces false matches.
- Page cache: a cold run and a warm run differ by an order of magnitude. The
  protocol must declare which one it wants and enforce it (`drop_caches` +
  `fsync`, or N warmups). Mixing them produces a bimodal distribution that then
  gets mistaken for noise.
- SSDs drift: internal GC, write cache, SLC buffer. Precondition and watch for
  inter-run drift, otherwise test ordering determines the result.

---

## 5. Family `net`

| Metric | What it measures | Backends | Modes | Targets | Cost | Det. |
|---|---|---|---|---|---|---|
| `net.tcp.connect` | connection setup latency | `bpf:fentry` tcp_v4/v6_connect | trace | pid, cgroup, system | low | low |
| `net.tcp.lifetime` | duration + volume per connection | `bpf:tp` sock:inet_sock_set_state | trace | pid, cgroup | low | low |
| `net.tcp.rtt` | RTT, cwnd, congestion state | `netlink` sock_diag · `bpf:fentry` on tcp_sock | poll, trace | pid, system | med | low |
| `net.tcp.retransmit` | retransmissions | `bpf:tp` tcp:tcp_retransmit_skb | trace, count | system, pid | low | low |
| `net.drops` | dropped packets (+ reason) | `bpf:tp` skb:kfree_skb (reason on recent kernels) | trace, count | system | low | medium |
| `net.bytes` / `.packets` | volume | `fs:proc` /proc/net/dev · `netlink` · `bpf:fentry` sock_sendmsg | poll, count | system, iface, pid, cgroup | free / low | low |
| `net.socket` | socket read/write latency, keyed by socket, task and interface | `bpf:fentry` sock_recvmsg/inet_sendmsg · `bpf:kprobe` | trace, count | pid, tid, cgroup, system | low / med | low |
| `net.request` | one declared request's life by state, and the network on either side of it | `bpf:usdt` request__start/done + sched_switch + tcp fentry · `bpf:usdt:threads` (no socket) | trace | pid, launch | low | medium |
| `net.backlog` | accept/SYN queue drops | `fs:proc` netstat (LINUX_MIB) | poll | system | low | medium |
| `net.qdisc` | TX scheduling latency | `bpf:tp` qdisc:qdisc_dequeue | trace | system | low | low |
| `net.softirq` | network processing time | `bpf:tp` irq:softirq_entry/exit | trace | cpu, system | med | low |
| `net.hwts` | NIC hardware timestamps | `SO_TIMESTAMPING` + PHC (per packet, no collector yet) · `netlink:ethtool` + PTP clock (what an interface can stamp, how far its clock is from ours) | trace, poll, snapshot | pid (socket opt), iface, system | free | high |
| `net.xdp` | in-driver counters | `bpf` XDP | count | iface | free | medium |
| `net.capture` | raw packets | `af_packet` TPACKETv3 | trace | iface | high | high |
| `net.iface` | errors, drops, pause frames | `netlink` ethtool -S | poll | iface | free / low | medium |

**Caveats**

- The key that crosses the whole network stack is `sock:cookie`
  (`bpf_get_socket_cookie`), not the PID: on receive, processing happens in
  softirq, outside any process context. `bpf_sk_storage` carries keys along the
  socket.
- `net.hwts` is the only way to settle "is it the network or is it us": it splits
  latency into NIC / kernel / application segments.
  That is the per-packet `SO_TIMESTAMPING` backend, which has no collector
  yet. What runs is `netlink:ethtool`, per interface, in `poll` or `snapshot`:
  whether a latency measured elsewhere is the card's word or the kernel's guess.
  It refuses `trace`, which would mean a stamp per packet.
- `net.bytes` emits `raw`: a series is an interface, and each event carries
  `rx` and `tx` beside its `bytes`, so the split by direction survives to the
  end of the run.
- `net.socket`'s `iface` is the interface the socket's cached route leaves by,
  read at the call: a key, so one series per interface. A unix socket, an
  unconnected datagram socket and a socket in another network namespace carry
  none.
- `net.request` follows only what the program declares --
  `request__start(id)` and `request__done(id)` USDT probes in the executable,
  Linux 6.6 or later (`uprobe_multi`); a program that declares none is
  refused with the probes it does carry. Its rows tile a request's life by
  state (on cpu, runnable, asleep, disk) plus `net_rx` and `net_tx`, read by
  the socket's cookie. `catalog/net.yaml` holds the rest of its caveats.
- `net.capture`: watch buffer drops. A capture that loses packets under load
  silently produces wrong conclusions.
- Multi-host introduces clock drift between machines, a different class of
  problem from local correlation. Out of scope for phase 1.

---

## 6. Family `sched` (scheduling and synchronization)

**The most underrated family**: it measures the time spent *not* running, which is
invisible to every CPU profiler.

| Metric | What it measures | Backends | Modes | Targets | Cost | Det. |
|---|---|---|---|---|---|---|
| `sched.runqlat` | run queue wait | `bpf:raw_tp` · `bpf:tp` sched_switch, read from the task's `sched_info` | trace | all | med | low |
| `sched.offcpu` | blocked time + stack | `bpf:tp:stacks` sched_switch + callchain · `bpf:raw_tp` · `bpf:tp` (no stack) | trace | pid, tid, cgroup, system | high / med | low |
| `sched.wakeup` | **wakeup chain** (who unblocks whom) | `bpf:raw_tp` · `bpf:tp` sched_wakeup (waker context) | trace | pid, tid, cgroup, system, launch | med | low |
| `sched.switches` | context switches | `pmu:sw` · `bpf:raw_tp` sched_switch (count, reached first for a cgroup) · `bpf:tp` | count, trace | all | free / low | low |
| `sched.migrations` | migrations | `bpf:raw_tp` · `bpf:tp` sched_migrate_task | trace, count | pid, cgroup, system | low | low |
| `sched.lock` | kernel lock contention | `bpf:raw_tp` · `bpf:tp` lock:contention_begin/end (≥6.1) · `bpf:kprobe` | trace | pid, tid, cgroup, system, launch | med / high | low |
| `sched.futex` | userspace futex wait | `bpf:raw_tp` · `bpf:tp` raw_syscalls (futex) | trace | pid, tid, cgroup, launch | high | low |
| `sched.mutex` | pthread contention | `preload` · `bpf:usdt` · `bpf:uprobe` pthread_mutex_* | trace | pid, tid | extreme | low |
| `sched.irq` | time in interrupt handlers | `bpf:raw_tp` · `bpf:tp` irq:irq_handler_entry/exit | trace, count | cpu, system | med | low |
| `sched.numa_balance` | automatic NUMA migrations | `bpf:raw_tp` · `bpf:tp` sched:sched_stick_numa etc. | trace, count | system | low | low |

**Caveats**

- `sched.wakeup` is the correlator's `WAKEUP` join mechanism: it is the only way
  to attribute latency **across a block** without instrumenting the application.
  Implement it early, the other families depend on it.
- A `sched.wakeup` event names both sides: `wakee_comm` and `waker_comm`
  (`comm` stays the wakee's). A wakeup issued from an interrupt carries
  `served_in_tid` and `served_in_comm`, the task the interrupt was served in,
  and `irq` (the hard interrupt's number, as `/proc/interrupts` numbers it) or
  `softirq` (the vector's name). A timer or an inter-processor interrupt has
  no `irq`.
- `sched.offcpu` with stacks is expensive on switch-heavy workloads: plan for
  in-kernel aggregation (stack-id → counter) rather than a raw trace.
- `lock:contention_*` is recent (≥ 6.1): fall back to futex + uprobes on older
  kernels.
- A process target is followed into every process it starts (a descendants
  map the fork tracepoint fills): `sched.offcpu`, `sched.runqlat`,
  `sched.wakeup`, `sched.switches` through `bpf:raw_tp`, `sched.migrations`,
  `sched.lock` and `sched.futex`; each of them takes a launched program as
  its target. A row keyed by task is still that task's;
  the run's totals include the children.
- A run-queue wait (`sched.runqlat`, and `sched.offcpu`'s `queued_ns`) is the
  kernel's own account, read from the task's `sched_info` at the switch that
  ends it (`rq->clock` less the queue stamp), so a task's waits add up to
  what its `schedstat` says. The tracepoints' account, wakeup to switch, is
  beside it as `wake_to_run_ns`.
- `catalog/sched.yaml` is the authority on each chain, its targets and its
  caveats; this table is the summary.

---

## 7. Family `power`

| Metric | What it measures | Backends | Modes | Targets | Cost | Det. |
|---|---|---|---|---|---|---|
| `power.energy` | pkg / cores / dram / gpu | `pmu` RAPL (power/energy-*/) · `fs:sys` powercap energy_uj | count, poll | **system / package** | free | medium |
| `power.freq` | effective frequency | `bpf:tp` power:cpu_frequency · `fs:sys` scaling_cur_freq · MSR aperf/mperf | poll, trace | cpu, system | free / low | low |
| `power.cstate` | idle state residency | `bpf:tp` power:cpu_idle · `fs:sys` | trace, poll | cpu | low / high | low |
| `power.temp` | temperatures | `fs:sys` hwmon | poll | system | low | low |
| `power.throttle` | thermal / PL1-PL2 throttling | `fs:sys` · MSR · `bpf:tp` thermal:* | poll, trace | system | free | medium |

**Caveats**

- This family is **also a confounder** for every other one: thermal throttling and
  AVX-512 frequency licensing silently corrupt CPU and GPU measurements. The
  harness must collect it **at all times**, even when the user did not ask for it,
  and **invalidate any throttled run**.
- RAPL is per package: `--pid` is invalid. Per-process attribution is a derived
  estimate (share of cycles) and must be labelled as such.
- `power.energy` emits `raw`: each reading's joules, per domain, kept to the
  end of the run, beside the summary, rate and timeline.

---

## 8. Family `gpu`

| Metric | What it measures | Backends | Modes | Targets | Cost | Det. |
|---|---|---|---|---|---|---|
| `gpu.job` | submit → fence (DRM job) | `bpf:tp` gpu_scheduler:drm_run_job / drm_sched_process_job | trace | system, pid | low | low |
| `gpu.engine_busy` | per-engine occupancy | `pmu` i915/xe/amdgpu · `vendor:nvml` | count, poll | system, launch | free / med | low |
| `gpu.freq` | clock frequency | `fs:sys` amdgpu / i915 · `vendor:nvml` | poll, snapshot | system, launch | free / high | low |
| `gpu.power` | energy drawn | `fs:sys` amdgpu hwmon · `vendor:nvml` | count, poll, snapshot | system, launch | free / high | low |
| `gpu.temp` | temperature | `fs:sys` amdgpu hwmon · `vendor:nvml` | poll, snapshot | system, launch | free / low | low |
| `gpu.mem` | VRAM allocated / used | `fs:sys` amdgpu · `vendor:nvml` | poll, snapshot | system, pid, launch | free / med | medium |
| `gpu.kernel` | GPU time per kernel | `vendor:cupti` (Activity) · `rocprof` · `l0` | trace | launch | extreme | medium |
| `gpu.api` | host-side submission latency | `preload` cuLaunchKernel / vkQueueSubmit | trace | pid | high | low |
| `gpu.counters` | counters inside a kernel, per launch, each with its verdict | `vendor:cupti` (range profiler, kernel replay) · `rocprofv3` | trace | launch | extreme | medium |
| `gpu.profile` | where time goes inside a kernel: sampled PCs, source lines, stall reasons | `vendor:cupti` (PC sampling) | trace | launch | med | low |

**Caveats**

- **NVIDIA proprietary: `gpu.job` and kernel PMUs are unavailable.** No DRM
  scheduler, no exposed PMU. Everything goes through `vendor:*`. AMD and Intel
  have the full kernel stack. The catalog must reflect that asymmetry rather than
  hide it.
- The polled sources -- `gpu.engine_busy`, `gpu.freq`, `gpu.power`,
  `gpu.temp`, `gpu.mem` -- emit `raw`, and a launch target sets their window,
  not their scope: the figure is the whole device's. Through NVML a read
  costs the tool's cpu in the driver, which is what the Cost cells say:
  `med` for occupancy and VRAM, `high` for a clock or a power reading, `low`
  for a temperature (each caveat in `catalog/gpu.yaml` gives the per-call
  figure).
- `gpu.job` measures a **command buffer**, not a kernel, IRQ included. It cannot
  tell you whether the GPU is saturating its SMs or waiting on VRAM.
- `gpu_scheduler` tracepoint fields are **volatile across versions**: discover the
  format at runtime, prefer `(fence_context, seqno)` over the pointer where it
  exists.
- GPU timestamps live in **another clock domain**: calibration is mandatory, and
  `ts_err_ns` is non-zero on all these events.
- `gpu.counters` does **not measure time** (kernel replay runs each launch
  several times): its value is counter determinism, judged per counter and per
  run (`reproducible here` / `not dominated by this workload`), never declared.
  A run asking for it and for `gpu.kernel`, or for it and `gpu.profile`, is
  refused. The clocks are reported, never locked: CUPTI locks them only when
  asked, and `omn` never asks. Other work on a shared device is counted with
  the program's; a launch whose window held more blocks than its grid is
  found, counted as `disturbed` and left out. A batch of launches whose ranges
  do not pair with them one for one (a CUDA graph's kernels among them) is
  counted as unpaired, none of it credited to a neighbour.
- `gpu.profile` samples a program counter per SM every `2^n` cycles: a share of
  samples is a share of SM cycles, placed on a source line when the kernel has
  line information (`-lineinfo`), by offset when it does not. Samples CUPTI
  counted and did not report are counted as lost.

---

## 9. Family `proc` (processes and syscalls)

| Metric | What it measures | Backends | Modes | Targets | Cost | Det. |
|---|---|---|---|---|---|---|
| `proc.syscall` | per-syscall latency and count | `bpf:tp` raw_syscalls:sys_enter/sys_exit | trace, count | pid, cgroup, system | extreme | low |
| `proc.exec` / `.exit` | process lifecycle | `bpf:tp` sched:sched_process_exec/exit | trace | system, cgroup | low | medium |
| `proc.fd` | open descriptors vs limit | `fs:proc` | poll | pid | low | medium |
| `proc.threads` | thread count | `fs:proc` | poll | pid | low | medium |
| `proc.signals` | signals received | `bpf:tp` signal:* | trace | pid | low | medium |

`proc.syscall` as a raw trace on a syscall-heavy workload is expensive
(`raw_syscalls` fires on *everything*): aggregate in the kernel, or filter by
syscall number inside the eBPF program.

---

## 10. Family `runtime` (late phase)

The layer above the kernel: GC, JIT, allocator, interpreter. Without it, a GC
pause or a JIT deoptimization gets attributed to the wrong resource.

| Metric | Backends | Availability |
|---|---|---|
| `runtime.gc` | `bpf:usdt` | depends on how the runtime was built (Java `+ExtendedDTraceProbes`, Python `--with-dtrace`, Node) |
| `runtime.jit` | `bpf:usdt` · perf-PID.map | same |
| `runtime.alloc` | `preload` · allocator stats | good |
| `runtime.custom` | arbitrary `bpf:uprobe` / `usdt` | user-declared |

Not a closed family: it is the extension point where users declare their own
probes.

---

## 11. Target matrix

| Target | Meaning | Inherently incompatible sources |
|---|---|---|
| `./prog` | launch and follow the process tree | — |
| `--pid` / `--tid` | existing process or thread | uncore, RAPL, device counters, `net.iface` |
| `--cgroup` | cgroup v2 (path) | uncore, RAPL |
| `--cpu` | core(s) | per-process sources |
| `--iface` | network interface | everything but `net.*` |
| `--system` | global | — |
| `--container` | bound to the cgroup its runtime names (`docker`, `podman`, `nerdctl`, or `OMN_CONTAINER_RUNTIME`), before resolution | same as cgroup |

**The container trap**: `--cgroup` is not a PID filter. Without the pid namespace,
events get attributed to the wrong process. Hence the `proc:pidns+pid` key: a
`tid` alone is ambiguous as soon as containers are involved.

**System-wide sources refuse filtering.** Returning a system-wide value while
presenting it as per-process attribution is the worst possible bug here: the
validator must reject the combination rather than silently degrade.

---

## 12. Mode × emit matrix

| Mode | Meaning | Valid emits |
|---|---|---|
| `count` | counter aggregated over the duration | `rate`, `summary`, `raw`, `topk` |
| `sample@freq` | periodic sampling | `stack`, `topk`, `hist`, `raw` |
| `trace` | every event | `raw`, `hist`, `topk`, `timeline`, `stack`, `rate`, `summary` |
| `poll@interval` | periodic re-read | `raw`, `timeline`, `summary` |
| `snapshot` | a single point-in-time read | `summary`, `raw` |

The table is `ModeKind::valid_emits` in `src/request.rs`, which is what the
validator checks.

**`count` has `raw` because a counted window *is* one event**: the counter, its
value, and the window the hardware says it was enabled for (`enabled_ns`,
`running_ns`). Without it a derived metric's terms -- the two counts behind
`cpu.ipc` -- and the counter's own window, the honest denominator for a rate,
never reach the user. **And `topk`**, for the same reason: a counted window is
one event per series, and ranking them is `rate` without the division -- a
counted run could otherwise say what its heaviest series' rate was and not
which series was heaviest. `topk:N@<attr>` ranks over an attribute's values
instead of the key's.

Invalid combinations must be rejected at validation time with an explicit
message: sampling a procfs file, counting a tracepoint without tracing it, asking
for stacks on a `poll`.

**`poll` has no `hist`, and that is load-bearing.** A histogram of a polled gauge
is a histogram of when the poller happened to look, which is a property of the
tick rather than of the program. It is also, under another name, the guard
against coordinated omission: a tick that ran late is visible as a gap in a
`timeline` and invisible once folded into a bucket, and the readings a late tick
loses are the ones from the moments that stalled -- the slow ones. `sample` and
`trace` do produce `hist` and do lose events, which is finding 37 of
`docs/open-findings.md` and is not closed.

---

## 13. Global options

```
--duration 30s              session duration
--budget 2%                 overhead ceiling, enforced by the runner: over it, a
                            cheaper backend of the chain or a stop, and it says which
--filter "bytes > 4096"     pushed into the kernel when possible, else userspace
--stack auto|sframe|fp+leaf|dwarf|lbr|fp|none
                            unwind mode; auto (the default) chooses by
                            machine and target, as a chain
--emit hist|raw|rate|topk|stack|timeline
--keys req,task,blk         correlation keys to attach
--backend force:<id>        escape hatch, for the comparison bench only
--out <dir>                 results + resolved file
--dump-request              print the resolved request and exit
--list [family]             catalog projection, filtered by actual availability
```

`--dump-request` is the primary debugging tool: it shows the selected backend,
where each filter landed, and what was refused or degraded.

---

## 14. Environment captured unconditionally

Collected on every session, without the user asking, and written into the
resolved file — this is what makes two measurements comparable across machines:

kernel and BTF · architecture and microarchitecture · speculative mitigations ·
clocksource · governor and frequency bounds · SMT state · THP mode · NUMA
topology · virtualization and steal time · vendor SDK versions · privileges
(`perf_event_paranoid`, BPF capabilities) · backends actually selected ·
**hardware prefetcher state**.

The last is captured and never changed, which is boundary 5. Turning the
prefetchers off is a real lever for isolating a memory effect and it is a
modification of the target, so whoever wants it does it themselves — and gets a
resolved file that says which state the numbers were taken in. Two runs on one
machine with different prefetcher settings are not comparable, and without this
line nothing would say why.

---

## 15. Suggested phasing

| Phase | Content | Why |
|---|---|---|
| **1** | `sched.wakeup`, `sched.offcpu`, `io.latency`, `net.tcp.lifetime`, `cpu.profile` | the full vertical case (HTTP request → file); forces clock, keys, join, emit |
| **2** | rest of `cpu`, `mem` (rss/faults/heap), extended `io`, `power` (guardrail) | coverage of the classic resources |
| **3** | advanced `mem` (bandwidth, c2c, wss), low-level `net`, `proc` | fine-grained diagnosis |
| **4** | `gpu`, `runtime`, `cpu.flow` | external dependencies, heavy decoders |

The ordering is not "simplest to hardest" but **"what forces the skeleton
first"**: phase 1 is chosen because it crosses five layers and imposes the entire
correlation architecture on a minimal perimeter.
