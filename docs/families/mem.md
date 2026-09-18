# `mem` — implementation notes

Owned by the `mem` thread. Record here what the measurements taught, the
traps that cost time, and anything the family needs from the shared layer.

## Findings

### The three footprints are one file read three ways

`mem.rss`, `mem.pss` and `mem.uss` all come out of `/proc/[pid]/smaps_rollup`,
and the kernel publishes no `Uss:` line: unique size is `Private_Clean +
Private_Dirty`, which is why the collector says on every event which file and
which lines it added up. The per-mapping `smaps` fallback sums the same fields
over every VMA and costs a walk of the whole address space.

### Two reads of procfs are two different instants, and the reads themselves grow the process

The first version of the test comparing `smaps_rollup` against the `smaps` walk
failed by 20 %, and the arithmetic was not at fault. Reading either file for the
first time faults in pages of the reader's own — buffers, the grown `String` —
so the second read legitimately sees a larger process. In a quiet process the
two agree to the kilobyte from the second round on.

Two consequences, both now in the tests. A comparison of two files brackets the
walk between a rollup taken before it and one taken after, rather than pretending
the two reads happened at once. A comparison of two *metrics* takes both from a
single read of one file, because reading twice would compare two moments of a
process that grows while it is being read.

### Retained memory is measured, not subtracted

The obvious formula for "what the allocator kept" is `RSS - live heap`, and it
is wrong in both directions at once. RSS also holds the executable, the stacks,
every file mapping and every anonymous mapping the program made for itself,
while live heap counts bytes the program asked for, which are not resident until
it writes to them. The difference of those two is a number made of four
unrelated things — the same shape as the `faults x page size` column that had to
be deleted earlier.

So `mem.heap.retained` measures the intersection instead: the shim says which
ranges were allocated and then freed, `/proc/[pid]/pagemap` says which pages of
those ranges are still in memory. A page shared between a freed range and a live
one is credited to the live side, which makes the number a lower bound by at
most one page per end of each range. RSS is reported beside it, for scale, and
is not part of it.

Measured on a program that allocates 1024 blocks of 64 KiB, touches every page,
and frees every other block: 30.1 MiB retained against 32 MiB freed, which is
the 15-of-16 pages the interior convention predicts for 64 KiB blocks.

### Freeing everything proves nothing

The first fragmentation test freed all 128 MiB it had allocated and measured no
retention at all, correctly: glibc consolidated the whole region into one top
chunk and trimmed it back to the kernel, and RSS fell from 133 MiB to 2 MiB.
An allocator gives memory back when the memory is at the top of the arena and
nothing is above it.

Fragmentation is holes, not volume. The test now frees every other block, which
leaves live blocks above the holes and makes the arena unshrinkable — the shape
a long-running service reaches after a few hours.

### An open `pagemap` answers for the address space the process had at the time

Opening `/proc/[pid]/pagemap` at attach time and keeping the descriptor returned
zero resident bytes for everything. The kernel binds the file to the process's
memory map when it is opened, and a program the tool launches replaces its
memory map when it execs — so the descriptor was answering for the address space
that existed before the program started, which contains none of its addresses.

Worse than wrong: quiet. A `pagemap` descriptor whose memory map is gone does
not fail, it stops returning data, and the scan counted the pages it never read
as absent. That is how a dead target became "zero retained memory", the one
answer this source must never invent. It now opens the file once per scan, two
syscalls a poll, and a read that stops early ends the run instead of shortening
the number.

### Draining and publishing are two different cadences

A session ticks as fast as its fastest collector needs, and a shim pipe needs
draining every ten milliseconds. Every other collector in the session was then
asked for a sample just as often, so a request for `poll@50ms` came back with a
series five times denser than it asked for — including `mem.rss`, which had been
published on the run's tick since it was written. Reading and publishing are now
separate: the collectors read whenever they are asked and publish on the grid
the request named.

## Needs from the shared layer

### `src/perf.rs`: the page-fault sampling test is flaky under a parallel suite

`perf::tests::a_page_fault_sample_carries_the_address_that_faulted` fails in
roughly two runs out of three of the full `cargo test --lib`, and passes every
time when run alone. It opens a *process*-scoped sampler
(`Scope::Process(0)`) with a 64-page ring while the test harness is running
other tests in other threads of the same process, so the ring fills with faults
from those threads: `lost()` stays zero and the assertion that 90 % of the
samples land inside the block this test touched fails.

Scoping the event to the current thread would make it measure what it claims to.
This is not a `mem` file, and it is failing at `130832a` before any of this
thread's changes, but it sits directly in the `mem` family's path: page faults
are a `mem` source, and a red suite that is red for an unrelated reason is a
suite nobody reads.

### `src/collect/mod.rs`: `cargo fmt --check` is not clean at `130832a`

`implemented()` has a chained method call `rustfmt` wants to break across lines.
One line, in a file this thread does not own, and it makes the repository's own
check command fail for every family.

### A run gives one heap source the shim, and the second one is refused

`mem.heap.alloc`, `mem.heap.live` and `mem.heap.retained` each open their own
pipe and each ask the launched program for `OMN_HEAP_FD`, so a session that asks
for two of them at once is refused by `merge_environments` with a message about
disagreeing environments — which is true, but it describes the mechanism rather
than the problem. They read the same record stream and should share one pipe.

What that needs: a per-run context passed to `Factory::build`, so that the
collectors of one run can hand each other a shared object instead of each
building its own. Something as small as

```rust
build: fn(&ResolvedRequest, &mut PrepareContext) -> Result<Box<dyn Prepared>, String>
```

where `PrepareContext` holds a `HashMap<&'static str, Box<dyn Any>>`, would let
this family register one shim stream per run and give every heap collector a
cursor over it. Until then, each of the three works alone, and
`mem.heap.retained` reports live bytes beside retained bytes so the useful pair
does not need two sources.

### A run binds one pid, so the machine-wide sources of this family cannot start

`run_all` resolves the target to a single pid and refuses anything else, so
`mem.thp`, `mem.swap` and `mem.pressure` — which are `system` sources reading
`/sys/kernel/mm/transparent_hugepage`, `/proc/vmstat` and
`/proc/pressure/memory` — have nowhere to attach, and `mem.cgroup` cannot take a
cgroup target either. All four are a few lines of parsing each; none of them can
be registered until a collector can be given a target that names no process.

What that needs: `Prepared::attach` taking the `Target` rather than a pid, with
each collector refusing by name the targets it cannot serve. That is four
sources of this family, and every source of `power`, plus most of `net`.

## What is still blocked, and on what

| Sources | Blocked on |
|---|---|
| `thp`, `swap`, `pressure`, `cgroup` | a target that is not a pid (above) |
| `cache_l1`, `cache_llc`, `cache_tlb`, `bandwidth`, `latency`, `sharing`, `numa` | the perf layer in counting mode, hardware events and uncore PMUs |
| `wss` | `page_idle` and DAMON probe classes |
| `page_alloc`, `kmalloc`, `reclaim`, `compaction` | the eBPF layer, Wave 2 |
| `heap.alloc` by call site | stack capture and symbolisation |
