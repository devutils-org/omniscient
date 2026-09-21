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

### Six sources that needed no privileges, and what each one is not

The cheapest breadth in the project, and the reason it kept being skipped is
that none of it is interesting until you write down what each number is *not*.

`mem.pressure` is the only source in this family that is not a quantity of
memory: it is time lost because there was not enough. `some` and `full` are not
two precisions of one number -- `some` is at least one task stalled, `full` is
every runnable task stalled, which is the machine having stopped -- and the gap
between them is the diagnosis. A poll divides the counter's increase by its own
window; a snapshot observes no window and reports the kernel's decayed averages
under their own names, which is the convention `io.pressure` set.

`mem.swap` measures the **flow**, not the stock. A cgroup that paged a gigabyte
out and read it all back has a flow of two gigabytes and a stock of nothing, and
`memory.swap.current` -- which is what the catalog's requirement names -- reports
the second. So the flow comes from `pswpin`/`pswpout`, in `/proc/vmstat` for the
machine and in the cgroup's own `memory.stat` for a cgroup, and the stock rides
along as `swapped_bytes`. Those two counters are in *pages* while almost every
neighbouring field in both files is in bytes or kilobytes; getting that wrong
scales the answer by the page size and looks entirely plausible.

`mem.thp` reports the PMD-sized huge page and says so on every event. Recent
kernels also allocate smaller multi-size huge pages -- 16 KiB through 1 MiB on
this machine -- and count each size in its own directory; adding them up would
give a "number of huge pages" that is a quantity of nothing, which is the same
mistake as reading a fault count as an amount of memory. The per-size statistics
arrived in 6.8, so the `fs:sys` backend requires `kernel>=6.8` and older kernels
fall through to `/proc/vmstat` -- which counts the same promotions and cannot see
the configured mode, which is exactly what its `degraded` line says.

`mem.cgroup` is not a footprint. Page cache the process read is charged there
and is in nobody's RSS; kernel memory -- stacks, page tables, slab -- is charged
there and appears in no userspace footprint at all; a page shared across cgroups
is charged once, to whoever faulted it first. It is also the number the OOM
killer compares against, which is why it is worth having beside the three
footprints rather than instead of them.

`mem.numa` through `numa_maps` is page *placement*, not accesses: a page on the
wrong node that is never touched costs nothing, and this backend cannot tell the
two apart. It emits one event per node, never a locality ratio computed from
placement.

`mem.wss` is the only one that changes what it measures, and it earns its place
anyway: resident size says how much memory a process has, and this says how much
of it the process uses. On the example session that ships with it, a program
holding 265 MiB and walking 16 MiB of them reports 265 MiB of RSS flat and
16-19 MiB of working set, every window.

### A zero-filled page is never written to a swap device

The best surprise of this stream, and it was found by a test disagreeing with a
hand-run of the same thing.

Forcing a cgroup to swap with a Python `bytearray` moves `memory.swap.current`
to 170 MB and leaves the cgroup's `pswpout` at **1**. The same arrangement with
a child that writes `0xA5` into every page moves `pswpout` by 41 110 pages --
160 MiB -- for the same 170 MB of stock. The kernel notices a page of zeros on
its way to being reclaimed, records that it was zero, and writes nothing: the
page is counted in `swpout_zero` instead, and read back from nowhere on the next
fault.

Two consequences. The flow and the stock disagreeing is not a bug, and
`mem.swap` says so in a caveat and carries `out_zero_bytes` beside `out_bytes`
so that the gap is visible rather than inferred. And a swap test that allocates
a `bytearray` measures nothing: it would pass a collector reading entirely the
wrong field. `Scratch::spawn_toucher` fills with `0xA5` for that reason, and
`spawn_zero_toucher` exists to make the difference the subject of its own test.

### A cgroup path is not a target, and the root cgroup is a different answer

`mem.cgroup --pid`, `mem.swap --pid` and `mem.pressure --pid` all resolve the
process's own cgroup out of `/proc/[pid]/cgroup`, because "what is this
container charged?" is the question people are asking when they name a process.
`0::/` is not a parse failure: it is a process nobody moved, and it resolves to
the hierarchy root like any other path.

The root is then refused, by name, for `mem.cgroup` alone: the kernel does not
charge the root cgroup and publishes no `memory.current` there. `memory.stat`
and `memory.pressure` *do* exist at the root, so the other two answer for the
machine through the cgroup file -- which is the same number by another route,
and the event says which file it came from.

### A working set cannot be measured without perturbing the thing measured

There is no counter for a working set. The kernel keeps a referenced bit per
page for its own reclaim decisions, and the only way to read a working set is to
clear the bit, wait, and see which pages got it back. Two consequences, both of
which reach the user rather than living here:

The clear is process-wide and visible. A second reader of the same process sees
windows it did not open, and there is no way to take this measurement privately.

The bit is reclaim's own input. Clearing it makes every page of the process look
cold until the process touches it again, so under memory pressure this is a
measurement that causes eviction. The run says so in a `note`, which reaches the
output beside the numbers, and the source says so in a caveat, which reaches the
user with the request.

The `page_idle` backend the catalog prefers has neither problem and cannot be
written for an unprivileged user at all: the bitmap is root-only, and an
unprivileged reader of `/proc/[pid]/pagemap` is handed zeroed page frame
numbers, which index nothing. It needs `CAP_SYS_ADMIN` twice over. That is why
`fs:proc:clear_refs` was added to the chain rather than `page_idle` being
implemented against a machine that cannot run it.

### The suite is the trap, again

Three of these tests were first written against the test binary's own address
space, and two of them failed for the same reason: `cargo test` runs its tests
in parallel threads of one process, and a dozen of them hold hundreds of
megabytes at once.

`mem.numa`'s first ground truth touched 64 MiB and watched the per-node
residency grow by 42 MiB, because other tests freed 22 MiB in between. It now
measures a forked child that touches a known amount and then holds still, and
checks two things that are not this code's arithmetic: the residency accounts
for what the child made resident, and summed over the nodes it equals the
`Rss:` the kernel publishes in `smaps_rollup` -- the same pages, counted by a
different subsystem through a different file.

`mem.wss`'s end-to-end test failed on its *quietest* window, which under a
loaded suite is a starved interpreter rather than a small working set. It now
compares the busiest interior window, which is the window the claim is actually
about, and excludes the last point outright: the program's exit truncates that
window, so it holds a fraction of an interval's work and is not a rate over one.

### Causing the thing, rather than waiting for it

Four of the six have ground truth that needed the condition to be *made*, and
systemd delegates enough to make it without privileges: `user@<uid>.service` is
owned by the user, so a cgroup created under it can be given a `memory.max` and
a process.

That is the whole apparatus behind these tests. A cgroup limited to 32 MiB with
a process touching 192 MiB inside it has to reclaim continuously -- which is
memory pressure by construction, and `mem.pressure` reads a stall where the same
collector on the same empty cgroup a moment earlier read none. The same
arrangement forces swap, and what `mem.swap` reports is then checked against two
other accountings of the same event: `memory.swap.current`, and the process's
own `Swap:` line in `smaps_rollup`, which is the same fact counted by a
different subsystem and keyed on the process rather than on the cgroup.

### Four sources now go through the kernel, and one event is not one measurement

`mem.page_alloc`, `mem.kmalloc`, `mem.reclaim` and `mem.compaction` land in the
sixth wave, through raw tracepoints with the histogram built in the kernel.
`src/bpf/mem_alloc.rs` counts what the two allocators hand out, per
`(task, cgroup, size bucket, direction)`; `src/bpf/mem_stall.rs` times what the
kernel does inside an allocating thread that cannot be given memory, per
`(task, cgroup, duration bucket, scope)`; `src/bpf/mem_program.rs` holds the
fragments both share.

The shape is `sched.runqlat`'s and the reason is the same one, with more force.
`mm_page_alloc` fires for every page the kernel hands anyone and `kmalloc` for
every object too small to be one: on an idle machine that is tens of thousands a
second, and under a fork storm it is a million. A ring record per event would
make the cost of the measurement grow with the thing being measured. What the
kernel keeps instead is a few hundred map rows per drain, each carrying the
allocations it stands for as `emit::WEIGHT_ATTR`, and the run says so in its own
output because every other source in this tool makes one event one measurement.

### A page allocation's bucket is free, and a slab allocation's is not

An allocation of order *k* is exactly `2^k` pages, and `emit::BucketLayout::Log2`
puts `2^k` in bucket `k + 1`. So for `mem.page_alloc` the order **is** the
bucket, plus one, with no search to run and no total to accumulate: every
allocation behind a row is the same size by construction, so the exact total is
`count << order` and userspace computes it without needing a shift instruction
the encoding does not have. A slab allocation's byte count is not a power of two,
so that one goes through the unrolled six-step `log2` and the kernel accumulates
both the bytes given and the bytes asked for. The difference between those two is
the allocator's rounding, which is a number no other source here can produce.

### The strongest ground truth this family has is a kernel counter

`mem.reclaim` reports `pages_reclaimed` off the `end` tracepoint's first
argument. A cgroup's own `memory.stat` reports `pgsteal_direct`. They are
incremented by different code in different places and they came out **equal to
the page**, on every run of `examples/mem-reclaim-squeeze.sh`: 146 114 against
146 114, then 92 833 against 92 833.

That is worth more than any of this family's other checks, and it is the shape
to copy: a measurement checked against a counter the kernel keeps for its own
reasons, rather than against a threshold somebody chose.

### The catalog's `mem.reclaim` names half of the source, and the half a cgroup run needs is the other one

Written up as D9 in `docs/scenarios/mem.md`. The short form: global reclaim and
cgroup-limit reclaim are two tracepoint pairs with one prototype, the entry
requires the first pair, and the entry also declares `cgroup: bpf_only` — so the
one target the chain names is the one the declared tracepoints cannot serve. A
build attached only to what the entry requires reports zero while a squeezed
cgroup reclaims ninety thousand pages.

Both pairs are attached, one program serves both beginnings, and each ending
writes its `scope` into the row: `global` or `cgroup`. An optional half that this
kernel did not publish would be reported in the run's notes rather than silently
missing, because zero episodes on a kernel with half the source is not the same
answer as zero on one with all of it.

### An allocation in interrupt context belongs to whoever was interrupted

Rows arrive keyed by `task:0`. `bpf_get_current_pid_tgid` returns the task that
is on the cpu, and a slab allocation made in softirq context is made on whatever
task the interrupt landed on — on an idle cpu, the per-cpu idle task. It is not
the idle task allocating; it is the kernel allocating with nobody to charge.
Left as it is, and said plainly in `docs/scenarios/mem.md` (D11), because an
allocation reported under the task that was interrupted is at least a true
statement about where it happened, and inventing an owner would not be.

### The frees are attached, counted, and not events

Both halves of each entry's chain are attached, so the totals are known. What
cannot be done is publish the frees as events beside the allocations: a series is
grouped by key, `direction` is neither a key nor a declared filter field, and a
histogram of allocations and frees together is a histogram of neither. They are
reported in the run's notes with their totals instead. `kfree` is worse and the
note says so — it carries the pointer and the call site and nothing that says how
large the object was.

### The map bound is small on purpose, so that something has reached it

Four thousand and ninety-six rows, a quarter of a megabyte, and deliberately not
the largest number that would fit. The heaviest ordinary machine-wide interval
measured here held a few hundred rows; a bound with five hundred times that
headroom is a loss path nobody would ever exercise. At 4 096 it is reachable by
`examples/mem-kernel-storm.sh` on this machine, reliably, and what happens when
it is reached is counted per cpu and surfaced as `dropped` with the capacity in
the reason.

### A cgroup target is matched over the drained rows, and the run says so

The programs write `bpf_get_current_cgroup_id()` into the row key, which is the
cgroup directory's inode number, so userspace turns a `--cgroup` path into a set
of numbers by walking the directory and its descendants. That is better than
comparing against one id in the program — a container's own cgroup usually has
children, and a task in a child is in the container by every definition a reader
means — but it is not the kernel-side filtering the resolver credits a `bpf`
backend with. The run says which it was, in the same words `sched.runqlat` uses
for its predicate.

## Needs from the shared layer

### `src/bpf/insn.rs`: two helper numbers and the log2 bucket

This family declares `bpf_get_current_comm` (16) and
`bpf_get_current_cgroup_id` (80) in `src/bpf/mem_program.rs`, because
`insn::helper` names the eight the layer used before it and `insn.rs` belongs to
nobody. Both are wanted by anything that identifies a task without a
`task_struct` in hand, which is every program attached to a tracepoint that does
not carry one.

```rust
pub const GET_CURRENT_COMM: i32 = 16;
pub const GET_CURRENT_CGROUP_ID: i32 = 80;
```

The unrolled `log2` is the larger request. `src/bpf/runqlat.rs` has one,
`src/bpf/mem_program.rs` has a second, and every source in this wave that wants
`--emit hist` off a kernel-built histogram will want a third: the same six-step
binary search, the same `emit::BucketLayout::Log2` numbering, the same
off-by-one that would put every measurement in the neighbouring bucket. It wants
to be one function in `insn.rs` with one test crossing it against the emit
layer's own definition, rather than one copy per family each checked separately.

```rust
/// Computes the log2 bucket of `value_reg` into r3, in BucketLayout::Log2's
/// numbering: zero is bucket zero, and bucket k holds [2^(k-1), 2^k).
pub fn push_log2_bucket(program: &mut Program, value_reg: u8);
pub fn bucket_low(bucket: u32) -> u64;
```

### `src/bpf/mod.rs`: four lines this stream had to add

`mod mem_alloc; mod mem_program; mod mem_stall;` and their `pub use`. There is
no way to register a program file without them, and every family stream in this
wave adds its own; they sort to different places in the two lists, so the merges
should not collide. Named here so the coordinator is not surprised by an edit to
a file the brief says nobody owns.


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

**Closed.** `Prepared::attach` takes the whole `Target`, and `mem.thp`,
`mem.swap`, `mem.pressure` and `mem.cgroup` are written against it: each refuses
by name the targets it cannot serve, and `mem.cgroup`'s refusal for a `system`
target names the source that *can* measure a process. What follows is kept for
the reasoning.


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

Twenty of this family's twenty-three sources now collect, and the three that do
not are blocked on mechanism rather than on effort. What is left, and why:

| Sources | Blocked on |
|---|---|
| `cache_l1`, `cache_llc`, `cache_tlb` | rows in `cpu/counters.rs`'s `SPECS` using `PERF_TYPE_HW_CACHE`. They are named `mem.*` and they belong to whoever owns the counters table; the mechanism decides. |
| `bandwidth` | uncore PMUs: a different sysfs layout and a different privilege story. Package-scoped, so `--pid` must be refused rather than degraded. |
| `latency`, `sharing` | PEBS load-latency and IBS: `perf_event_open` sampling with a data-source payload nothing in this build decodes yet. |
| `numa` through `pmu` | the same PEBS data-source information. The `fs:proc` fallback is written and says plainly that it measures placement rather than accesses. |
| `wss` through `page_idle` and DAMON | `CAP_SYS_ADMIN`, twice over for `page_idle` (see above), and a DAMON probe class nothing answers. The `fs:proc:clear_refs` backend serves an unprivileged user today. |
| `heap.alloc` by call site | stack capture, which exists now; wiring the shim's frames through it has not been done. |
