# Stream heap, the twenty-fifth wave

Branch `feat/w25-heap`. Done sequentially by one agent, in the main checkout.

## Outside the owned files, said first

- `src/bpf/syscall.rs` (shared): `link_uretprobe_multi`, the uprobe-multi
  link with `BPF_F_UPROBE_MULTI_RETURN`; `link_uprobe_multi` unchanged.
- `src/stack.rs` (unwind's): `symbol_file_offset`, a function's file offset
  by `.symtab`/`.dynsym`.
- `src/bpf/mod.rs`: `pub mod mem_heap` (a new program file: `mem_alloc.rs`
  is the page and slab allocator's program, not the heap's).
- `examples/mem-refused-not-built.yml` and `tests/scenario_mem.rs` (triage's
  and mem's): the not-built backend they pin is `bpf:usdt` now that
  `bpf:uprobe` is written.

## What landed

1. **`mem.heap.alloc` on `bpf:uprobe`** (heap 2): `src/bpf/mem_heap.rs` (five
   programs: three entries stamping the size per thread, one return closing
   it with the address and the user stack, one `free` entry) and
   `src/collect/mem/heap/bpf.rs`, which makes the preload backend's events
   (`alloc::event_of`, extracted) with `stack_id` on each allocation.

## Shown vs reasoned

Reasoned: the programs build against this kernel's BTF; this test process's
C library defines `malloc`, `calloc`, `realloc` and `free` at offsets found;
the record, `calloc`'s product and `realloc`'s split are unit tested; the
events are checked equal in shape to the preload backend's. Not linked here:
no CAP_BPF.

## Privileged runs wanted

- `omn trace mem.heap.alloc --backend force:bpf:uprobe -e summary -- python3 -c 'b=[bytearray(1<<20) for _ in range(64)]'`
  beside the same through `preload`: the 64 MiB in both totals;
  `-e stack` names the allocating Python frames' native callers.
- A static binary the shim cannot reach (`musl-gcc -static`), where the
  uprobe is the only road.

## Not done

1. `mem.heap.live` and `mem.heap.retained` on the uprobes: the records are
   the same; their collectors read a preload `Stream`, and taking a uprobe
   feed is a second constructor each.

## Tests

`--lib collect::mem::heap mem_heap`, `--test scenario_mem --test family_mem
--test cli --test catalog_model`, clippy, fmt. On this machine, as on `main`,
`scenario_mem::a_stack_of_every_process_is_refused_by_the_build_and_the_rest_is_served`
fails for want of per-cpu perf events; the two that pinned the uprobe as
unbuilt now pass (they failed on `main` here).
