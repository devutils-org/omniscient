# Scenarios — the advice rules

What each rule of `catalog/advice.yaml` was measured to do, on the
development machine (Linux 7.2, 24 cpus, 64 GiB, a 4 GiB zram swap, cgroup v2
with `cpu memory pids` delegated to the user's systemd). Each rule's ground
truth is a test of `tests/advice_truth.rs`: a fixture
(`tests/fixtures/advice/fixture.c`) the rule fires on, the setting changed in
the fixture or in the systemd scope it runs in -- never on the machine -- and
a second run `--against` the first. A rule is in the table only if its
prediction moved that way.

The tests run one at a time (`alone()`): the OOM victims are traced
machine-wide, so a kill in a parallel test is in the window, and the
cpu-bound runs are timed.

---

## `cgroup.memory.swap-denied`

`grow 300` (300 MiB of incompressible memory) in a scope with
`MemoryMax=128M`:

| Scope | Outcome |
|---|---|
| `MemorySwapMax=0` | killed in 0.1 s; `mem.oom` counts 1, the rule holds |
| swap allowed | completes in 0.7 s, the excess in zram; `mem.oom` counts 0: as predicted |

**The rule the brief planned is not this one, and the measurement is why.**
The brief proposed `memory.high` before `memory.max`, "turns kills into
stalls". On this machine:

| Scope | Outcome |
|---|---|
| `MemoryMax=128M`, swap allowed, one thread or sixteen | never killed: zram absorbs the excess |
| `MemoryMax=128M -p MemoryHigh=110M`, swap allowed | never killed either, a little slower |
| `MemoryMax=128M -p MemorySwapMax=0` | killed in 0.1 s |
| `MemoryMax=128M -p MemorySwapMax=0 -p MemoryHigh=100M` | **killed after 260 s** of throttling |

With somewhere to put the memory, `memory.max` did not kill, so there was
nothing for `memory.high` to prevent; with nowhere, `memory.high` did not
prevent the kill and turned a 0.1 s failure into four minutes of stall. Advice
to set it would have been wrong here. What the measurement supports is the
swap the cgroup was denied, and the `memory.high` result is a caveat of that
rule.

## `oom.victim-by-size`

`oomtwo`: a `keeper` holding 120 MiB and a `grower` that grows to 100 MiB,
in `MemoryMax=256M`, swap denied.

| `grower`'s `oom_score_adj` | First victim |
|---|---|
| 0 | `keeper`, the larger: the rule holds (`proc.exec` counts 3 programs) |
| 1000 | `grower`: as predicted |

The victims are read machine-wide (`mem.oom --system` in trace mode): on a
launched program the tracepoint sees that process alone, and one run observes
one workload, so the workload's cgroup cannot be asked beside it. The rule
says so. The adjustment is set on the grower, not on the launched program, so
`--against` reports the target's own `oom_score_adj` unchanged beside the
verdict rather than crediting a change it did not see.

The first version used the last victim: the keeper's death left too little
for the grower at 150 MiB, both died, and the last victim was the grower on
both sides. It reads the first victim, and the grower stops at 100 MiB.

## `sched.shared-cpu`

`spin 4 200000000` (four threads, a fixed amount of work each):

| Affinity | Run-queue latency, mean | Wall clock |
|---|---|---|
| `taskset -c 0` | 6.8 ms: the rule holds | 768 ms |
| `taskset -c 0-3` | 5.6 us: as predicted | 193 ms |

## `io.readahead-refused`

`readseq FILE 64 HINT`: a 64 MiB file of incompressible bytes, evicted from
the cache by its owner (`POSIX_FADV_DONTNEED`), then its first half read 4 KiB
at a time.

| Hint | `io.readahead` ahead | Page-cache misses |
|---|---|---|
| `POSIX_FADV_RANDOM` | -3: the rule holds | 8192 |
| none | 1533: as predicted | 9728 |

Two attempts failed before this one, and each is a property of the sources:

- **A whole file read to its end says nothing**: pages filled and pages
  looked up agree whatever readahead did, and `ahead` was 0 for both hints
  (`io.readahead`'s own caveat). The fixture reads half.
- **A file of one repeated byte is not read from the disk**: btrfs compressed
  64 MiB into about 2 MiB of extents, and `io.size` saw 512 requests of 4 KiB
  for either hint. The fixture writes incompressible bytes.

The wall clock moved both ways across repeated runs (348 ms and 217 ms with
the hint, 130 ms and 204 ms without), so it is a side effect the rule
reports, not a prediction the test holds.

## Not built, and why

- **THP compaction stalls** (`mm.thp.defrag`, `MADV_HUGEPAGE`): the fixture
  would have to fragment memory reliably, which a user process on a 64 GiB
  machine with 35 GiB free does not. A guest under `distroproof run` can.
- **The governor and EPP, `vm.swappiness`, IRQ affinity, `tcp_notsent_lowat`,
  `read_ahead_kb` per device**: each needs a machine-wide write, which only a
  distroproof guest may make in a test.
