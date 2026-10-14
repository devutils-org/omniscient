# `mem`: how much memory, who allocated it, and is the machine short

Lazy default: `omn mem -- ./prog` (`mem.rss` polled every 100 ms; `--cost med`
adds `mem.faults`, `--cost high` adds `mem.pss`).

## Three questions that sound alike

"How much memory does it use" has several right answers, and they differ:

| You want | Source | What it counts |
|---|---|---|
| what `top` shows as RES | `mem.rss` | resident pages, a shared page counted in full in every process mapping it |
| a share that sums across processes | `mem.pss` | shared pages divided by the number of mappers |
| what would be freed if it died | `mem.uss` | private pages only |
| what the OOM killer compares | `mem.cgroup` | the cgroup's charge: page cache, kernel memory, every process in it |
| what it asked `malloc` for | `mem.heap.live` | bytes allocated and not freed; not resident memory |
| memory the allocator keeps after `free` | `mem.heap.retained` | fragmentation, the number that predicts OOMs |
| pages it actually touches | `mem.wss` | the working set over a window |

`malloc` allocates nothing resident: pages become resident on first write. An
allocation profile and an RSS curve never line up, and joining them is what
answers "which allocation cost memory" (`make demo`, question 3).

## Which source for which question

| Question | Source | Mode |
|---|---|---|
| Is it growing? | `mem.rss` (cheap: `statm`; full: `smaps_rollup`) | `poll@100ms -e timeline` |
| Who allocates, from where? | `mem.heap.alloc` | `trace -e stack` or `topk` |
| Is it leaking? | `mem.heap.live` beside `mem.rss` | `poll@100ms` |
| Fragmentation? | `mem.heap.retained` | `poll` |
| Page faults, and where? | `mem.faults` | `count`, `sample -e stack` |
| Cache misses, TLB misses? | `mem.cache_l1`, `mem.cache_llc`, `mem.cache_tlb` | `count` |
| Memory access latency, NUMA locality, false sharing? | `mem.latency`, `mem.numa`, `mem.sharing` | `sample` (PEBS or IBS) |
| Is the machine short of memory? | `mem.system` (what `free` prints), `mem.pressure` (PSI) | `poll@1s` |
| Is it swapping? | `mem.swap` | `poll` |
| Direct reclaim and compaction stalls? | `mem.reclaim`, `mem.compaction` | `trace -e hist` |
| OOM kills | `mem.oom` | `count` |
| Kernel allocations | `mem.page_alloc`, `mem.kmalloc` | `trace -e topk` (system-wide) |
| Transparent huge pages | `mem.thp` | `poll` |

## Examples

```console
omn trace mem.rss -m poll@100ms -e timeline,summary -- ./prog
omn trace mem.rss,mem.pss,mem.uss -m poll@500ms -e summary --pid 1234 -d 10s
omn trace mem.heap.alloc -m trace -e topk:10 -- ./prog
omn trace mem.heap.alloc -m trace -e stack --format folded -o out -- ./prog
omn trace mem.faults -m count -e summary -- ./prog
omn trace mem.system,mem.pressure -m poll@1s -e timeline --system -d 1m
omn trace mem.reclaim -m trace -e hist --system -d 30s
```

## Targets

- The footprints (`mem.rss`, `mem.pss`, `mem.uss`) take a launch or `--pid`
  (`mem.rss` and `mem.pss` also `--cgroup`).
- **`mem.heap.*` need a program the tool launches**: the preload shim reaches a
  program only at exec. A statically linked program, or one with its own
  allocator, ignores it, and the run says no allocation was seen rather than
  that none happened.
- `mem.cgroup`, `mem.pressure`, `mem.swap` and `mem.oom` accept a `--pid` but
  **report its cgroup**: the kernel accounts these per cgroup, never per
  process. The root cgroup has no `memory.current`, so a process nobody moved
  into a cgroup is refused for `mem.cgroup`.
- `mem.system`, `mem.thp`, `mem.bandwidth`, `mem.kmalloc`, `mem.compaction` are
  the machine's alone.

## Backends and cost

- `mem.rss` through `smaps_rollup` is `med` and carries PSS and USS to check
  against; `statm` is `free` and gives resident pages only. Under `--budget` the
  run moves from one to the other and says so.
- `mem.heap.*` and `sched.mutex` are `extreme`: the shim runs in every
  allocation. Its 3 MiB record ring is mapped into the target, so up to 3 MiB of
  what `mem.rss` reads beside it is the tool's own.
- `mem.latency`, `mem.numa` (by access) and `mem.sharing` need PEBS (Intel) or
  IBS (AMD); a virtual machine rarely has either.
- `mem.wss` through `clear_refs` changes what it measures (it clears the
  kernel's referenced bits); `damon` is the cheap road where the kernel has it.

## Traps

- THP makes RSS grow in 2 MiB steps in the background: freeze and log the mode
  (`mem.thp`).
- A poll misses a peak shorter than its period; `mem.heap.live` carries
  `peak_bytes` so the peak is not lost.
- PSI absent (kernel without `CONFIG_PSI`) is not zero pressure.
