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
| `extreme` | > 10× (replay, DBI) |

Cost is **throughput-dependent**: a tracepoint at 1 kHz is `low`, the same one at
2 MHz is `high`. The resolver must be able to switch `trace` → in-kernel `hist`,
or `trace` → `sample`, when the observed rate exceeds the budget.

**Evidence levels** (carried by every event and every span)

`IDENTITY` (propagated key) > `KEY` (shared typed key) > `WAKEUP` (wakeup chain)
> `TEMPORAL` (overlap only, a hypothesis).

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

---

## 2. Family `cpu`

| Metric | What it measures | Backends | Modes | Targets | Cost | Det. |
|---|---|---|---|---|---|---|
| `cpu.cycles` | unhalted cycles | `pmu` (HW_CPU_CYCLES) | count, sample | all | free / low | medium |
| `cpu.instructions` | retired instructions | `pmu` (HW_INSTRUCTIONS) | count, sample | all | free | **high** |
| `cpu.ipc` | derived cycles/instr | — | count | all | free | medium |
| `cpu.branch_misses` | mispredicted branches | `pmu` | count, sample | all | free | medium |
| `cpu.cache_misses` | LLC misses | `pmu` (HW_CACHE_MISSES) | count, sample | all | free | low |
| `cpu.topdown` | TMA: front-end / back-end / bad-spec / retiring | `pmu` (slots + topdown-*) | count | all | low | medium |
| `cpu.profile` | on-CPU profile with stacks | `pmu:sw` (CPU_CLOCK) or `pmu` cycles + callchain | sample@freq | all | low-med | low |
| `cpu.oncpu_time` | effective CPU time | `bpf:tp` sched_switch · `fs:proc` · `fs:cgroup` cpu.stat | trace, poll | pid, cgroup, system | low / free | low |
| `cpu.context_switches` | context switches | `pmu:sw` | count, sample | pid, cgroup, cpu | free | low |
| `cpu.migrations` | cross-core migrations | `pmu:sw` (CPU_MIGRATIONS) | count | pid, cgroup | free | low |
| `cpu.util` | utilization by state | `fs:proc` /proc/stat · `fs:cgroup` cpu.stat | poll | system, cpu, cgroup | free | low |
| `cpu.pressure` | PSI CPU stall | `fs:proc` /proc/pressure/cpu · cgroup cpu.pressure | poll | system, cgroup | free | low |
| `cpu.throttle` | CFS cgroup throttling | `fs:cgroup` cpu.stat (nr_throttled) | poll | cgroup | free | medium |
| `cpu.flow` | exhaustive control flow | `pmu:aux` intel_pt / cs_etm | trace | pid, cpu | high | high |

**Caveats**

- `cpu.instructions` is the most stable metric of the lot (typically CV < 0.1 %)
  but **never bit-exact**: interrupt skid, speculation, IRQs, SMT. Strict
  determinism belongs to DBI (cachegrind), outside the kernel-level core.
- `cpu.topdown` is **heavily microarchitecture-dependent**. The catalog must map
  it per CPU family rather than hard-code event names.
- `cpu.profile` with stacks: the unwind mode must be declared (`fp` / `dwarf` /
  `lbr`). `dwarf` copies the stack on every sample → far higher volume and cost.
- `cpu.flow`: the kernel provides raw bytes, decoding is a separate concern
  (libipt + binary images + sideband). A late-phase deliverable.
- Disabling SMT or pinning affinity shifts counters by 10-30 %: capture it in the
  resolved environment.

---

## 3. Family `mem`

| Metric | What it measures | Backends | Modes | Targets | Cost | Det. |
|---|---|---|---|---|---|---|
| `mem.rss` / `.pss` / `.uss` | resident footprint | `fs:proc` smaps_rollup → smaps | poll, snapshot | pid, cgroup | free | medium |
| `mem.cgroup` | cgroup accounting | `fs:cgroup` memory.current / .peak / .stat | poll | cgroup | free | medium |
| `mem.faults` | minor/major page faults | `pmu:sw` PAGE_FAULTS_MIN/MAJ · `bpf:tp` | count, sample, trace | all | free / low | medium |
| `mem.page_alloc` | kernel page allocations | `bpf:tp` kmem:mm_page_alloc / mm_page_free | trace | system, cgroup | med | medium |
| `mem.kmalloc` | slab allocations | `bpf:tp` kmem:kmalloc / kfree | trace | system | med | medium |
| `mem.heap.alloc` | userspace allocations by site | `preload` → `bpf:uprobe` malloc/free · `bpf:usdt` | trace | pid | med / high | high |
| `mem.heap.live` | live heap | `preload` (delta) · jemalloc/tcmalloc stats | poll | pid | low | high |
| `mem.heap.retained` | **fragmentation**: retained / live | derived (`heap.live` vs `rss`) | poll | pid | low | high |
| `mem.cache_l1` / `.llc` / `.tlb` | misses per level | `pmu` (L1-dcache-load-misses, LLC-*, dTLB-*) | count, sample | all | free | low |
| `mem.latency` | load latency | `pmu` PEBS load-latency · AMD IBS | sample | all | med | low |
| `mem.sharing` | **false sharing** (HITM) | `pmu` PEBS HITM · IBS | sample | all | med | low |
| `mem.bandwidth` | DRAM throughput | `pmu` uncore IMC · Intel RDT/MBM | count | **system only** (MBM: cgroup) | free | low |
| `mem.numa` | access locality | `pmu` PEBS data-source · `fs:proc` numa_maps | sample, poll | all | med | low |
| `mem.wss` | real working set | `fs:sys` DAMON · page_idle | poll | pid, cgroup, system | low | medium |
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
| `io.latency` | submit → completion (device) | `bpf:tp` block_rq_issue/complete · `fs:tracefs` · `fs:proc` diskstats | trace, count | system, cgroup, pid* | low | low |
| `io.queue` | insert → issue (software wait) | `bpf:tp` block_rq_insert/issue | trace | system, cgroup | low | low |
| `io.bytes` / `.iops` | volume and throughput | `bpf:tp` · `fs:proc` diskstats · `fs:cgroup` io.stat | count, trace, poll | system, cgroup | free-low | low |
| `io.size` | request size distribution | `bpf:tp` block_rq_issue | trace | system, cgroup | low | medium |
| `io.merges` | request merges | `bpf:tp` block_rq_merge · diskstats | count | system | free | medium |
| `io.inflight` | queue depth | `fs:proc` diskstats | poll | system | free | low |
| `io.syscall` | latency as the application sees it | `bpf:tp` raw_syscalls · `bpf:fentry` vfs_read/write | trace | pid, cgroup | low-med | low |
| `io.fs` | VFS/FS latency | `bpf:fentry` vfs_* · ext4/xfs/btrfs tracepoints | trace | pid, cgroup | low | low |
| `io.pagecache` | page cache hit/miss | `bpf:tp` filemap:mm_filemap_add_to_page_cache + faults | trace | pid, system | med | low |
| `io.readahead` | readahead effectiveness | `bpf:tp` readahead tracepoints | trace | system | low | low |
| `io.writeback` | deferred writeback | `bpf:tp` writeback:* | trace | system, cgroup | low | low |
| `io.fsync` | sync latency | `bpf:fentry` vfs_fsync · ext4_sync_file | trace | pid | low | low |
| `io.pressure` | PSI I/O | `fs:proc` /proc/pressure/io | poll | system, cgroup | free | low |

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
| `net.tcp.rtt` | RTT, cwnd, congestion state | `netlink` sock_diag · `bpf:fentry` on tcp_sock | poll, trace | pid, system | free-low | low |
| `net.tcp.retransmit` | retransmissions | `bpf:tp` tcp:tcp_retransmit_skb | trace, count | system, pid | low | low |
| `net.drops` | dropped packets (+ reason) | `bpf:tp` skb:kfree_skb (reason on recent kernels) | trace, count | system | low | medium |
| `net.bytes` / `.packets` | volume | `fs:proc` /proc/net/dev · `netlink` · `bpf` socket | poll, count | system, pid, cgroup | free | low |
| `net.socket` | socket read/write latency | `bpf:fentry` sock_sendmsg/recvmsg | trace | pid, cgroup | low | low |
| `net.backlog` | accept/SYN queue drops | `fs:proc` netstat (LINUX_MIB) | poll | system | free | medium |
| `net.qdisc` | TX scheduling latency | `bpf:tp` qdisc:qdisc_dequeue | trace | system | low | low |
| `net.softirq` | network processing time | `bpf:tp` irq:softirq_entry/exit | trace | cpu, system | low-med | low |
| `net.hwts` | NIC hardware timestamps | `SO_TIMESTAMPING` + PHC | trace | pid (socket opt) | free | high |
| `net.xdp` | in-driver counters | `bpf` XDP | count | iface | free | medium |
| `net.capture` | raw packets | `af_packet` TPACKETv3 | trace | iface | high | high |
| `net.iface` | errors, drops, pause frames | `netlink` ethtool -S | poll | iface | free | medium |

**Caveats**

- The key that crosses the whole network stack is `sock:cookie`
  (`bpf_get_socket_cookie`), not the PID: on receive, processing happens in
  softirq, outside any process context. `bpf_sk_storage` carries keys along the
  socket.
- `net.hwts` is the only way to settle "is it the network or is it us": it splits
  latency into NIC / kernel / application segments.
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
| `sched.runqlat` | run queue wait | `bpf:tp` sched_wakeup → sched_switch | trace | pid, cgroup, cpu | med | low |
| `sched.offcpu` | blocked time + stack | `bpf:tp` sched_switch + callchain | trace | pid, cgroup | med-high | low |
| `sched.wakeup` | **wakeup chain** (who unblocks whom) | `bpf:tp` sched_wakeup (waker context) | trace | pid, system | med | low |
| `sched.switches` | context switches | `bpf:tp` · `pmu:sw` | count, trace | all | free-low | low |
| `sched.migrations` | migrations | `bpf:tp` sched_migrate_task | trace, count | pid, cgroup | low | low |
| `sched.lock` | kernel lock contention | `bpf:tp` lock:contention_begin/end (≥6.1) | trace | pid, system | med | low |
| `sched.futex` | userspace futex wait | `bpf:tp` raw_syscalls (futex) | trace | pid | med | low |
| `sched.mutex` | pthread contention | `preload` → `bpf:uprobe` pthread_mutex_* | trace | pid | med-high | low |
| `sched.irq` | time in interrupt handlers | `bpf:tp` irq:irq_handler_entry/exit | trace | cpu, system | low-med | low |
| `sched.numa_balance` | automatic NUMA migrations | `bpf:tp` sched:sched_stick_numa etc. | trace | system | low | low |

**Caveats**

- `sched.wakeup` is the correlator's `WAKEUP` join mechanism: it is the only way
  to attribute latency **across a block** without instrumenting the application.
  Implement it early, the other families depend on it.
- `sched.offcpu` with stacks is expensive on switch-heavy workloads: plan for
  in-kernel aggregation (stack-id → counter) rather than a raw trace.
- `lock:contention_*` is recent (≥ 6.1): fall back to futex + uprobes on older
  kernels.

---

## 7. Family `power`

| Metric | What it measures | Backends | Modes | Targets | Cost | Det. |
|---|---|---|---|---|---|---|
| `power.energy` | pkg / cores / dram / gpu | `pmu` RAPL (power/energy-*/) | count, poll | **system / package** | free | medium |
| `power.freq` | effective frequency | `bpf:tp` power:cpu_frequency · `fs:sys` scaling_cur_freq · MSR aperf/mperf | poll, trace | cpu, system | free | low |
| `power.cstate` | idle state residency | `bpf:tp` power:cpu_idle · `fs:sys` | trace, poll | cpu | low | low |
| `power.temp` | temperatures | `fs:sys` hwmon | poll | system | free | low |
| `power.throttle` | thermal / PL1-PL2 throttling | `fs:sys` · MSR · `bpf:tp` thermal:* | poll, trace | system | free | medium |

**Caveats**

- This family is **also a confounder** for every other one: thermal throttling and
  AVX-512 frequency licensing silently corrupt CPU and GPU measurements. The
  harness must collect it **at all times**, even when the user did not ask for it,
  and **invalidate any throttled run**.
- RAPL is per package: `--pid` is invalid. Per-process attribution is a derived
  estimate (share of cycles) and must be labelled as such.

---

## 8. Family `gpu`

| Metric | What it measures | Backends | Modes | Targets | Cost | Det. |
|---|---|---|---|---|---|---|
| `gpu.job` | submit → fence (DRM job) | `bpf:tp` gpu_scheduler:drm_run_job / drm_sched_process_job | trace | system, pid | low | low |
| `gpu.engine_busy` | per-engine occupancy | `pmu` i915/xe/amdgpu · `vendor:nvml` | count, poll | system | free-low | low |
| `gpu.freq` / `.power` / `.temp` | hardware state | `pmu` · `fs:sys` · `vendor:nvml` | poll | system | free | low |
| `gpu.mem` | VRAM allocated / used | `fs:sys` amdgpu · `vendor:nvml` | poll | system, pid | free | medium |
| `gpu.kernel` | GPU time per kernel | `vendor:cupti` (Activity) · `rocprof` · `l0` | trace | pid | med | medium |
| `gpu.api` | host-side submission latency | `preload` cuLaunchKernel / vkQueueSubmit | trace | pid | low | low |
| `gpu.counters` | internal counters (SM, cache, stalls) | `vendor:ncu` · `rocprofv3` | snapshot | pid | **extreme** | **high** |

**Caveats**

- **NVIDIA proprietary: `gpu.job` and kernel PMUs are unavailable.** No DRM
  scheduler, no exposed PMU. Everything goes through `vendor:*`. AMD and Intel
  have the full kernel stack. The catalog must reflect that asymmetry rather than
  hide it.
- `gpu.job` measures a **command buffer**, not a kernel, IRQ included. It cannot
  tell you whether the GPU is saturating its SMs or waiting on VRAM.
- `gpu_scheduler` tracepoint fields are **volatile across versions**: discover the
  format at runtime, prefer `(fence_context, seqno)` over the pointer where it
  exists.
- GPU timestamps live in **another clock domain**: calibration is mandatory, and
  `ts_err_ns` is non-zero on all these events.
- `gpu.counters` does **not measure time** (replay destroys the timeline): its
  value is counter determinism. It locks clocks by default (`--clock-control base`
  in `ncu`) — to be aligned with the harness protocol.

---

## 9. Family `proc` (processes and syscalls)

| Metric | What it measures | Backends | Modes | Targets | Cost | Det. |
|---|---|---|---|---|---|---|
| `proc.syscall` | per-syscall latency and count | `bpf:tp` raw_syscalls:sys_enter/sys_exit | trace, count | pid, cgroup, system | med-high | low |
| `proc.exec` / `.exit` | process lifecycle | `bpf:tp` sched:sched_process_exec/exit | trace | system, cgroup | free | medium |
| `proc.fd` | open descriptors vs limit | `fs:proc` | poll | pid | free | medium |
| `proc.threads` | thread count | `fs:proc` | poll | pid | free | medium |
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
| `--container` | resolved to cgroup + pidns | same as cgroup |

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
| `count` | counter aggregated over the duration | `rate`, `summary` |
| `sample@freq` | periodic sampling | `stack`, `topk`, `hist`, `raw` |
| `trace` | every event | `raw`, `hist`, `topk`, `timeline` |
| `poll@interval` | periodic re-read | `raw`, `timeline`, `summary` |
| `snapshot` | a single point-in-time read | `summary` |

Invalid combinations must be rejected at validation time with an explicit
message: sampling a procfs file, counting a tracepoint without tracing it, asking
for stacks on a `poll`.

---

## 13. Global options

```
--duration 30s              session duration
--budget 2%                 overhead budget; the resolver degrades and says so
--filter "bytes > 4096"     pushed into the kernel when possible, else userspace
--stack fp|dwarf|lbr|none   unwind mode
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
(`perf_event_paranoid`, BPF capabilities) · backends actually selected.

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
