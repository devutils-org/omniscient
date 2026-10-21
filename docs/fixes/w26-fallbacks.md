# w26 fallbacks — eBPF sources served from the kernel's own counters

Phase 1 of the twenty-sixth wave (`BRIEF.md`, "Decided before this wave":
no backend for a source that has no honest one).

## Landed

- `src/collect/counter_file.rs`: a source's counts out of a kernel counter
  file, described as a `Spec` (which file, its layout -- `name value` lines,
  paired snmp sections, `/proc/stat`'s per-cpu tick columns --, which
  counters sum into which attribute, which is the value). Count and poll
  through `collect::poll`; the machine as the one target.
- Four rows, each held to the eBPF backend of its own source over one
  window (`tests/fallbacks.rs`, both requests in one session):

| Source | Backend | From | Measured against the eBPF backend |
|---|---|---|---|
| `mem.page_alloc` | `fs:proc` | `/proc/vmstat` `pgalloc_*`, `pgfree` | 104 796 / 104 917 pages, 105 698 / 105 019 |
| `sched.irq` | `fs:proc` | `/proc/stat` `irq` + `softirq` ticks | 140 ms / 130 ms over 2 s (10 ms ticks) |
| `sched.migrations` | `pmu:sw` | the software counter `cpu.migrations` reads | 336/338, 210/217, 155/157 |
| `net.tcp.retransmit` | `fs:proc` | `/proc/net/snmp` `Tcp:RetransSegs` | served, one machine event (no retransmission provoked) |

## Decided, source by source: no honest file fallback

The brief listed candidates; reading each against its catalog entry left
these eBPF-only, because the file holds a different quantity:

- `mem.reclaim`, `mem.compaction`, `io.writeback`, `net.softirq`: the entries
  measure time (`ns`); `/proc/vmstat` and `/proc/softirqs` hold counts. A count
  presented as a stall would be the worst bug this tool can have, a number
  about something else. (`/proc/stat`'s softirq time is every vector, not the
  network's, so it is not `net.softirq`'s either.)
- `sched.numa_balance`: its value is the scheduler's swap and move
  decisions; vmstat's `numa_*` count page faults and migrations, `mem.numa`'s
  quantity.
- `net.drops`: no file holds the kernel's drops with their reasons; the
  counters that exist (`softnet_stat`, `snmp` discards) are a few of them.
- `io.queue`, `io.size`: per request; diskstats' averages would answer
  `io.util` and `io.latency`'s questions, which have them.
- `proc.*`, `sched.wakeup`, `sched.runqlat`, `sched.futex`, `net.tcp.*`
  events: one event per occurrence, which no counter file keeps. `proc.exec`
  through the proc connector needs `CAP_NET_ADMIN`, a grant this project does
  not ask for.
