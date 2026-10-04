# What omn replaces: the `heap` family

The tools: `valgrind --tool=dhat`, `valgrind --tool=massif`, `heaptrack`,
`memleak` (bcc or libbpf-tools), against `mem.heap.alloc`, `mem.heap.live`
and `mem.heap.retained`. Every pair runs one workload,
`tests/reference/workloads/heap/blocks.py 64 1048576 4 0.3`: 64 blocks of
1 MiB from libc's malloc, the peak held 0.3 s, all freed but 4. Pairs in
`catalog/references/heap.yaml`, by id; every verdict below is the shared
runner's (`cargo test --test reference pairs_heap`, 2026-10-04: 8 held,
1 skipped). Alternate runs
throughout: valgrind replaces malloc itself, and omn's shim and heaptrack's
are both `LD_PRELOAD` interposers, so no two of them can watch one run.

**In short:** the four totals a heap tool closes with -- bytes and calls
allocated, the peak, what was never freed -- `omn` gives, on this workload
equal to the byte against dhat and massif and within 0.2 % of heaptrack. Where
every one of these tools is used for -- *which call site* -- `omn` serves
nothing today: the preload backend captures no stack, and the source that
says "by call site" refuses `-e stack` only after the run.

| Tool and use | omn equivalent | Covered | Same numbers | Missing |
|---|---|---|---|---|
| `valgrind --tool=dhat`: total bytes allocated | `omn trace mem.heap.alloc --filter 'op == "alloc"' -e topk:1` (`total`) | full | yes: 73,637,336 both through the runner (pair `dhat-total-bytes`, rel 0.001) | a sum in `summary` (read from `topk` today) |
| `valgrind --tool=dhat`: blocks allocated | same, `events` | full | yes: 3,186 both (pair `dhat-total-blocks`, rel 0.01) | -- |
| `valgrind --tool=dhat`: `At t-gmax`, the peak live heap | `omn trace mem.heap.live -m poll@50ms -e summary` (`max`) | part | yes: 68,908,064 both (pair `dhat-gmax`, rel 0.01), **only because the workload holds its peak** | a high-water mark: a poll misses a peak shorter than its period (60.5 MB of 68.9 MB at `poll@1ms` without the hold) |
| `valgrind --tool=dhat`: `At t-end`, never freed | same, `last` | part | yes: 4,605,465 both (pair `dhat-end`, rel 0.005; dhat with `--run-libc-freeres=no`) | the block count at exit (dhat: 52 blocks); `mem.heap.live` is bytes only |
| `valgrind --tool=dhat`: per-site tree, reads and writes per block, lifetimes, `dh_view` | none | none | no pair: omn reads no access to a block and keys no allocation by site | stacks on `mem.heap.alloc`; block lifetimes (alloc to free); access counts are out of reach of an interposer |
| `valgrind --tool=massif`: peak snapshot | `omn trace mem.heap.live -m poll@50ms -e summary` (`max`) | part | yes: 68,908,064 both (pair `massif-peak`, rel 0.01) | the same high-water mark as above |
| `valgrind --tool=massif`: heap over time (`ms_print`'s graph) | `omn trace mem.heap.live -m poll@50ms -e timeline` | part | no pair: massif's time axis is instructions executed, omn's is the monotonic clock; no common abscissa to compare | -- |
| `valgrind --tool=massif`: the allocation tree at the peak | none | none | no pair | live bytes keyed by call site |
| `heaptrack`: calls to allocation functions | `omn trace mem.heap.alloc --filter 'op == "alloc"' -e topk:1` (`events`) | full | within 0.2 %: 3,186 vs 3,180 (pair `heaptrack-calls`, rel 0.01) | -- |
| `heaptrack`: peak heap memory consumption | `omn trace mem.heap.live -m poll@50ms -e summary` (`max`) | part | within 0.11 %: 68,908,064 vs 68.98M (pair `heaptrack-peak`, rel 0.01); heaptrack prints two decimals of MB and reads 72 KB higher than dhat and massif | the high-water mark |
| `heaptrack`: total memory leaked | same, `last` | full | within 0.1 %: 4,605,465 vs 4.61M (pair `heaptrack-leaked`, rel 0.01) | -- |
| `heaptrack`: temporary allocations (freed right after) | none | none | no pair | an allocation's lifetime, or its free paired with it |
| `heaptrack`: per-site tables, flame graph, `heaptrack -p PID` | none | none | no pair | stacks on `mem.heap.alloc`; attaching to a running program (the shim reaches a program only at exec) |
| `memleak -c prog`: outstanding bytes | `omn trace mem.heap.live -m poll@50ms -e summary` (`last`) | part | pair `memleak-outstanding` (rel 0.01) **skips here**: memleak needs root and `sudo -n` is not granted | -- |
| `memleak -p PID`: outstanding by stack, on a running program | none | none | no pair | stacks; attach by pid |
| `memleak` without `-p`: kernel allocations | `mem.kmalloc` | out of this family | no pair (the bpftrace stream's) | -- |
| (none of them) fragmentation, retained by the allocator | `omn trace mem.heap.retained -m poll@50ms` | omn only | no pair: no tool here measures it (massif's `mem_heap_extra_B` is padding and headers, not freed pages kept); on this workload 131,072 bytes, as it should be, glibc having unmapped the 1 MiB blocks it got by `mmap` | -- |
