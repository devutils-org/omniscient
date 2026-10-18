# Stream mem, the twenty-fifth wave

Branch `feat/w25-mem`. Done sequentially by one agent, in the main checkout.

## Outside the owned files, said first

- `src/bpf/btf.rs` (shared layer): `Btf::struct_size`, one additive
  function, its own commit (`fix(btf): ...`). `mm_struct.rss_stat` is an
  array of `percpu_counter`, whose size depends on the kernel's
  configuration.
- `src/bpf/proc_events.rs` (proc's): `EventStream::load`, `drain` and
  `publish` made `pub(super)`, so `mem_oom.rs` reuses the ring, the drop
  counter and the loader instead of copying them.
- `src/request.rs` (the `KeyKind` seam): `cache`, a derived key read off the
  `cache` attribute.
- `CATALOG.md`: the `mem.oom` row and a `mem.slab` row (`tests/cli.rs` checks
  the cost column against the chain).

## What landed

1. **Defect: `mem.oom -m trace` landed on `/proc/vmstat` and failed at the
   attach.** The counting backends now declare `modes: [count, poll]`, so a
   trace request without CAP_BPF is refused by the resolver, naming the
   program it needs.
2. **`mem.oom`'s victims** (bpftrace 5): a `bpf:raw_tp` backend on
   `oom:mark_victim` (`src/bpf/mem_oom.rs`, `src/collect/mem/victims.rs`),
   one event per victim: `pid`, `tid`, `comm`, `uid` (passed since 6.8, said
   when absent), `oom_score_adj`, and `total_vm`, `rss`, `anon_rss`,
   `file_rss`, `shmem_rss`, `swap` in bytes. Both `rss_stat` layouts (6.2+
   `percpu_counter[]`, earlier `mm_rss_stat`) are read from BTF.
3. **`mem.slab`**: `/proc/slabinfo` as slabtop prints it, one event per cache
   keyed `cache`, the value slabtop's CACHE SIZE.
4. **Keys**: `level` on `mem.latency`; `func`, `within`, `insn` on
   `mem.cache_*`; `user` on `mem.rss`, `mem.pss`, `mem.uss`; `mapping` on
   `mem.pss` and `mem.uss` through `fs:proc:smaps` (one event per mapping
   name).

## Shown vs reasoned

- `mapping` (shown): `mem.pss -m snapshot -k mapping` of a `sleep` read
  `libc.so.6` 57 KiB, `/usr/bin/sleep` 48 KiB, `ld-linux-x86-64.so.2`
  17 KiB, the numbers `pmap -X` printed for the same pid. A unit test checks
  that the rows add up to the whole file's `Pss`.
- `user` (shown): `user:moha` on the same process; `/etc/passwd` lookup unit
  tested.
- `func` on `mem.cache_l1 -m sample` (shown): 204 samples of a Python
  shuffle-and-index loop ranked by function (unnamed libpython offsets, the
  binary has dynamic symbols only).
- `level` (reasoned): IBS needs per-cpu perf events, refused at paranoid 2
  here. The attribute was already on the events.
- Victims (reasoned): the program builds against this kernel's BTF (7.2:
  `mark_victim(task_struct *, uid_t)`, `percpu_counter` layout), and three
  unit tests check the record, the target comparison and the counter offsets.
  The attach needs CAP_BPF.
- `mem.slab` (reasoned): the parser is checked on three lines of a real
  file; the file is root's here and the run is refused with that reason.

## Not done

1. Nothing of the stream's list is left. `perfrecord.yaml`'s `load-latency`
   `why` is reference's file (done there, if that stream runs).

## Privileged runs wanted

- The victims: in a scope with no swap,
  `systemd-run --user --scope -p MemoryMax=32M -p MemorySwapMax=0 python3 -c 'b=bytearray(128<<20)'`
  under `omn trace mem.oom -m trace -e raw --system -d 10s`; compare with
  `journalctl -k | grep 'Killed process'` (pid, comm, uid, anon-rss,
  file-rss, shmem-rss, oom_score_adj).
- `mem.slab`: `sudo -n omn trace mem.slab -m snapshot -e raw` against
  `sudo -n slabtop -o -s c | head -20` (CACHE SIZE per cache).

## Tests

`--lib collect::mem bpf::mem_oom request`, `--test family_mem --test
scenario_mem --test catalog_model --test lazy --test cli`, clippy, fmt.
Three `scenario_mem` tests fail, and fail the same way on `main` on this
machine (`a_source_this_build_does_not_collect_exits_three_and_says_what_is_missing`,
`a_stack_of_every_process_is_refused_by_the_build_and_the_rest_is_served`,
`the_three_refusals_are_told_apart`): they expect exit 3 from sources this
machine refuses with exit 1 for want of CAP_BPF and per-cpu perf events.
`every_mode_and_emit_of_the_counter_sources_...` failed once under load and
passed alone.

## Decision lines for `DECISIONS.md`

- A source whose backends answer different questions by mode (counts, or
  victims) declares each backend's `modes:`, so a mode lands only on a
  backend that serves it.
