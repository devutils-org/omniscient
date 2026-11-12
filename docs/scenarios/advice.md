# Scenarios — the advice rules

What each rule of `catalog/advice.yaml` was measured to do, on the
development machine (Linux 7.2, 24 cpus, 64 GiB, a 4 GiB zram swap, cgroup v2
with `cpu memory pids` delegated to the user's systemd). Each rule's ground
truth is a test of `tests/advice_truth.rs`: a fixture
(`tests/fixtures/advice/fixture.c`) the rule fires on, the setting changed in
the fixture or in the systemd scope it runs in -- never on the machine -- and
a second run `--against` the first. A rule is in the table only if its
prediction moved that way.

The tests run one at a time (`alone()`): the cpu-bound runs are timed, and
one test traces the machine's victims on purpose.

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

The victims are the workload's own: `mem.oom`'s trace backend follows the
processes a launched program starts, as the scheduler's sources do, so the
keeper and the grower are both its. The first version read them machine-wide,
because the tracepoint then compared the launched pid alone and a cgroup
target beside a launch is refused; `the_victims_are_the_launched_tree_s_and_no_one_else_s`
holds the fix both ways -- a child killed is named, and a kill in another
scope 0.6 s into the window, which a `--system` trace of the same window sees,
is not. The adjustment is set on the grower, not on the launched program, so
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

| Hint | Block requests per page-cache miss | `io.readahead` ahead |
|---|---|---|
| `POSIX_FADV_RANDOM` | 8192 / 8192 = 1.0: the rule holds | -3 |
| none | 306 / 9728 = 0.03: as predicted | 1533 |

The evidence is the requests per miss. Three attempts failed before it, and
each is a property of the sources:

- **A whole file read to its end says nothing**: pages filled and pages
  looked up agree whatever readahead did, and `ahead` was 0 for both hints
  (`io.readahead`'s own caveat). The fixture reads half.
- **A file of one repeated byte is not read from the disk**: btrfs compressed
  64 MiB into about 2 MiB of extents, and `io.size` saw 512 requests of 4 KiB
  for either hint. The fixture writes incompressible bytes.
- **`ahead` itself is too weak a sign**: a reader with readahead on ends with
  one window in front of it, 58 pages on ext4 at 128 KiB, 30 on btrfs at
  128 KiB, and one with it off still had 10 on btrfs. A threshold between
  them made the hint rule fire on an Arch guest that had no hint. One block
  request per missed page is unambiguous: 1.0 against 0.001 to 0.03.

The wall clock moved both ways across repeated runs here (348 ms and 217 ms
with the hint, 130 ms and 204 ms without), so it is a side effect the rule
reports, not a prediction the test holds.

## `bdi.readahead-off` (in a guest)

`distroproof-advice.toml`, as root, Arch (7.2, btrfs) and Debian 13 (6.12,
ext4), 2 GiB, 4 cpus. Every backing device's `read_ahead_kb` set to 0, the
same `readseq`, then each put back:

| Guest | Requests per miss, at 0 -> put back | Wall clock |
|---|---|---|
| Arch, btrfs | 1.0 -> 0.0014: as predicted | 334 ms -> 50 ms |
| Debian 13, ext4 | 1.0 -> 0.032: as predicted | 354 ms -> 164 ms |

**The setting is the bdi's, not the disk's.** The first run set
`/sys/block/vda/queue/read_ahead_kb` to 0: on Debian's ext4 that is the same
knob, and the rule held; on Arch's btrfs, readahead went on (1546 pages
ahead), because a btrfs filesystem has a backing device of its own
(`/sys/class/bdi/btrfs-N`, 4096 KiB there) that the disk's queue does not
govern. `settings.yaml` reads `bdi.read_ahead_kb` for every bdi, and the
rule names the instance at 0.

## `mm.thp-defrag-stall` (in a guest)

The same guests, `enabled` at always. `fragment` touches 95 % of what is
available in small pages (`MADV_NOHUGEPAGE`) and gives back every other one;
`huge 600` then faults 600 MiB 2 MiB at a time.

| Guest | `defrag` | Compaction (`mem.compaction`, the machine's) | The kernel's `/proc/vmstat` |
|---|---|---|---|
| Arch | always | 137 ms: the rule holds | 206 `compact_stall` |
| Arch | madvise | none: as predicted | 0 more stalls, 197 `thp_fault_fallback` |
| Debian 13 | always | 111 ms: the rule holds | 216 `compact_stall` |
| Debian 13 | madvise | none: as predicted | 0 more stalls, 189 `thp_fault_fallback` |

The wall clock went down on Arch (166 -> 17 ms) and up on Debian (170 ->
203 ms): taking small pages is not free either, and the rule reports it as
a side effect, not a prediction.

The first two runs proved nothing, and the vmstat lines are why the script
keeps printing them: fragmenting 60 % then 85 % of the memory left enough
untouched memory contiguous for every huge page (`thp_fault_alloc` up by
154, no fallback, no stall). At 95 %, with more faulted than is left
contiguous, every fault compacts.

## Not built, and why

- **The governor and EPP**: a QEMU guest has no cpufreq, so the setting it
  would change is not there; a hardware cell is where it can be checked.
- **`vm.swappiness`, IRQ affinity, `tcp_notsent_lowat`**: each needs a
  machine-wide write and a workload whose prediction is one-sided, which no
  fixture here has yet.

## `tcp.unsent-backlog` (in a network namespace)

`backlog 48`: 48 MiB written in 64 KiB chunks over loopback to a reader that
takes one chunk every 5 ms, each chunk stamped when it is handed to
`write()`. `tcp_notsent_lowat` belongs to the network namespace, and the root
of a user namespace may set its own, so the test runs both answers under
`unshare -rn` on the development machine, changing nothing of the host's
(`advice_truth::a_low_unsent_mark_shortens_the_backlog`):

| `tcp_notsent_lowat` | Unsent bytes, most seen | Longest from `write()` to `read()` | Wall clock |
|---|---|---|---|
| 4294967295 (the default) | 2 490 880: the rule holds | 215 to 225 ms | 4.0 s |
| 131072 | 196 502: as predicted | 30 to 32 ms | 4.0 s |

The wall clock is the reader's either way: the bytes wait in the program
instead of in the kernel, and the rule says so. What the mark bounds is the
queue the next write joins: 128 KiB, and a chunk past it, which is why the
most seen is 192 KiB and not 128. The chunk still waits about 30 ms, in the
reader's receive buffer, which no sender setting reaches.

`net.tcp.rtt` gained `launch` among its targets for this: a launched program
is a pid by the time the collector attaches, and the netlink backend already
narrowed to a pid's descriptors.

