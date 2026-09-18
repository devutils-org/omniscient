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
