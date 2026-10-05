# The twenty-fourth wave, stream mem

Branch `feat/w24-mem`. The defect first, then the features, in the order the
brief lists them. No shared layer was touched: everything is in
`catalog/mem.yaml`, `src/collect/mem/`, `tests/family_mem.rs` and
`docs/scenarios/mem.md`. `src/perf.rs` and `src/bpf/attach.rs` are called,
not changed.

## What landed

1. **Defect: `mem.latency` delivered about 70 % of the rate asked, silently**
   (perfrecord, w23). `ibs.rs` now keeps a run clock of the followed tasks
   (per-thread `schedstat`, a cpu's busy time in `/proc/stat`, a cgroup's
   `cpu.stat`) and its first note says the delivered rate and its share of
   the rate asked, and, under 90 %, why. The cause is the unit with `omn`'s
   two format bits (`cnt_ctl` and `swfilt`) in frequency mode, shown with
   `perf record` alone (table in `docs/scenarios/mem.md`, D17). A catalog
   caveat says it.
2. **The `peer` half of the same finding is not a defect**: `perf script -F
   data_src` reads the same words as *"core, same node Any cache hit"*; a
   unit test pins six real data-source words of this machine against perf's
   decoding. The `level` caveat says how the share moves and why the pair's
   populations differ (cycles between tags against ops).
3. **`mem.system`** (new, `system.rs`): `/proc/meminfo` as `free` prints it,
   slab totals and split included (sysstat 4, 6).
4. **`mem.numa` on `fs:sys`**: the machine's per-node `numastat` counters, a
   system target (sysstat 7).
5. **`mem.oom`** (new, `oom.rs`): kills for the machine (`/proc/vmstat`) or
   a cgroup (`memory.events`: `kills`, `oom`, `oom_group_kill`, `max`).
6. **Per-process swap-in** (`swapin.rs`): `mem.swap` gains `perf:kprobe`, a
   counting kprobe on `swap_read_folio`, for pid, tid and launch targets;
   last in the chain, chosen with `--backend force:perf:kprobe`.
7. **VIRT and SHR** on `mem.rss` (top 3), and the rollup's `Swap`,
   `AnonHugePages`, `Locked` (top 4's fields; not its per-mapping key).
8. **The heap**: `peak_bytes` and `peak_allocations` on `mem.heap.live`
   (heap 3), live blocks were already `allocations` (heap 5), `lifetime_ns`
   on `mem.heap.alloc`'s frees (heap 6).
9. **`mem.sharing`'s per-line detail** (perfrecord 6, which the brief calls
   "c2c's per-line detail"): each contested line's offsets, instructions,
   loads and stores in the note; `offset` and `op` on every IBS event.
10. **`mem.faults`' early open**: a caveat with the numbers (perfstat 6).

## Shown vs reasoned

Shown on this machine (Ryzen 9 3900X, family 17h, one node, 24 cpus, under
the wave's load):

| What | omn | independent | how |
|---|---|---|---|
| IBS delivered rate, `sample@4000hz` | 2 658-2 973 /s (66-74 %) | `perf record -e ibs_op/cnt_ctl=1,swfilt=1/u -F 4000`: 2 640 /s | same workload, same format bits |
| the run clock | 2.32 s, 2.48 s | runner's `observed_cpu_ns` 2.319 s, 2.483 s | read interval 0; at 100 ms, 1-3 % short |
| `peer` decode | `peer` | perf: "core, same node Any cache hit" | same words, unit test |
| `mem.system` | total 67 330 080 768, buff/cache 34 422 685 696 | `free -b`: same, to the byte | used within 4 MB |
| `mem.oom` | 1 kill, `max` >= 1 | the child ended on SIGKILL | unit test, scratch cgroup |
| swap-ins per process | 275 898 in 3 s; 133 040 / 115 872 / 122 829 a second | scope's `pswpin` 346 333 in 3.4 s; 134 438 / 120 358 / 122 331 | integration test through the binary |
| VIRT, SHR, swap | `statm`, rollup | `status`' `VmSize`, `RssFile+RssShmem`, `VmSwap`, equal | unit test |
| heap peak | 67 108 864 B in 64 blocks | the program: 64 x 1 MiB, held 2 ms (every poll read 0) | manual run |
| `mem.sharing` detail | line 0x4040c0, +0 +8, 1 instruction, 0 stores | `perf c2c report`: 0x0, 0x8, 0x401230, no store refs | same workload, separate runs |
| `mem.faults` offset | 60-62 | `perf stat -e page-faults`: 57-58 | `/bin/true`, three runs each |

Reasoned only: `mem.numa --system`'s deltas (the arithmetic is unit-tested;
no workload stands out of the machine's 50 000 pages a half-second); the
lifetimes' values (paired by pointer, plausible, not held to a clock); the
cgroup and cpu run clocks of `mem.latency` (the thread clock is the one
shown).

## Tests run

- `cargo test --lib collect::mem` 149 tests: 148 passed, and
  `footprint::tests::both_backends_read_a_plausible_and_similar_resident_size`
  failed once in that parallel run and passed alone 3 of 3 (it reads the test
  binary's own RSS while sibling tests run; not new, worth the flaky tally).
- `cargo test --lib catalog` 63 passed (after merging main).
- `cargo test --test family_mem` 17 passed; `cargo test --test scenario_mem`
  42 passed.
- `cargo clippy --workspace --all-targets -- -D warnings` clean,
  `cargo fmt --check` clean.
- `swapin::tests::a_thrashing_walker_s_swap_ins_are_its_cgroup_s` skips in a
  unit-test binary (a kprobe needs `CAP_PERFMON`, which a rebuilt test binary
  cannot carry); its integration twin in `tests/family_mem.rs` runs.

## Not done, and why

Ranked:

1. **`level` as a key on `mem.latency`** (perfrecord 5): a key needs a
   `KeyValue` variant in `src/event.rs`, which is not an append seam (only
   `KeyKind` is). Needs from stream emit.
2. **Mapping and user keys** (top 4, 8): the same, a `KeyValue::Mapping` and
   a `KeyValue::User`; the collectors would then fill them from `smaps` and
   `status`' `Uid`.
3. **The heap's eBPF attach** (heap 2): a uprobe program on `malloc`/`free`
   in a new `src/bpf/mem_heap.rs` is a stream of its own; not started.
4. **`mem.oom`'s victim** (bpftrace 5's "victim, trigger"): a program on
   `oom:mark_victim` in `src/bpf/mem_*.rs`; the counts landed, the victim
   did not.
5. **Per-cache slab** (`slabtop`): `/proc/slabinfo` is root-only; a backend
   would need the privileged run below.
6. **The `load-latency` pair** (`catalog/references/perfrecord.yaml`, stream
   reference's): its `why` still says "58 %" and does not know the cause;
   the request is below.

## Privileged runs wanted

- The per-cache slab, for a future `mem.slab` pair: `sudo -n slabtop -o -s c
  | head -20` (not granted today: `slabtop` is in neither sudoers file).

## Decision lines

- A rate in Hz on a sampling source is a target the kernel adjusts towards;
  a run states the rate it delivered over the time the followed tasks ran,
  never over the wall.
- A per-process count the kernel keeps only per cgroup is read through a
  counting kprobe on the function that does the work, with no program
  loaded, and is last in its chain: the source's default for a pid stays the
  cgroup's flow, in bytes.

## Needs from another stream

- **emit** (`src/event.rs`, `src/request.rs`): `KeyValue::Level`,
  `KeyValue::Mapping`, `KeyValue::User` and their `KeyKind`s; then `mem`
  fills them.
- **reference** (`catalog/references/perfrecord.yaml`, `load-latency`): the
  `why` can now say the cause: the IBS unit with `cnt_ctl` and `swfilt`
  settles at about 66-74 % of 4000 Hz in frequency mode (perf alone with the
  same format terms: 2 640 /s), and `peer` is perf's "core, same node Any
  cache hit"; the means differ because perf counts cycles between tags.
  `omn`'s results now carry the delivered rate in their first note.
- **coordinator**: `docs/open-findings.md` can close the "IBS delivered
  rate" finding as fixed-by-stating; `CATALOG.md` and `man/omn.1` gain
  `mem.system`, `mem.oom`, `mem.numa --system`, `mem.swap`'s `perf:kprobe`.
