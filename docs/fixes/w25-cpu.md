# Stream cpu, the twenty-fifth wave

Branch `feat/w25-cpu`. Done sequentially by one agent.

## Outside the owned files, said first

- `src/perf.rs` (the stream's exception): `PERF_SAMPLE_PERIOD` asked in
  frequency mode and decoded after `cpu`, `Sample::period`.
- `src/stack.rs` (unwind's): `prime_kernel_symbols_aside` and
  `kernel_symbols_read`, two functions beside `prime_kernel_symbols`.
- `src/request.rs` (the seam): `KeyKind::Insn`, appended.

## What landed

1. **Defect: `cpu.context_switches -m count -e rate`** divided by the
   counter's enabled time, the task's time on a cpu. The per-task and
   per-cgroup counts of switches and migrations now carry it as `oncpu_ns`,
   so the rate is over the run's window, as `sched.switches` was fixed.
2. **`PERF_SAMPLE_PERIOD`**: a frequency-mode sample's `count` is the period
   the kernel chose for it, and `period` travels beside it; the note and the
   caveat say a share is perf's and the sum estimates the count.
3. **`/proc/kallsyms` read beside the window**: `cpu.profile` with a kernel
   half starts the read on a thread of its own and opens its window at once;
   the first drain to meet a kernel frame before the read ends waits for it.
4. **`cpu.profile`'s provenance**: `perf_maps` from the unwinders (path,
   device, inode) and `executables` from each process's unwinder, which
   follows an `execve`.
5. **`--keys insn`**: `insn:<func>+0x<off>`, the leaf instruction, put on
   each sample with a stack.
6. **`func`, `within` and `insn`** in the `keys:` of every cpu source that
   samples with stacks.
7. **`mem.cache_l1`, `mem.cache_llc` (`pmu`, `pmu:generic`),
   `mem.cache_tlb`** sample on their miss events (`profile::SAMPLED`).
8. **Caveats**: `cpu.oncpu_time`'s `fs:proc` reap (no longer true), the
   `pmu` row of `cpu.profile` as a pointer kept on purpose.

## Shown vs reasoned

- Period weights (shown): `cpu.instructions -m sample@997hz -e topk` summed
  6 186 074 381 over 432 samples; `-m count` read 6 191 363 259 (0.09 %).
  `mem.cache_l1` sampled 40 331 034, counted 40 319 522.
- Provenance (shown): `python3 -X perf` wrote `/tmp/perf-125350.map`;
  `perf_maps` read `dev 48, ino 6983`, and `stat -c '%d %i'` said `48 6983`.
- `insn` (shown): a C loop's samples fell on `hot+0x56` (divsd), `+0x5a`
  (addsd, the divide's skid), `+0x5e`; `objdump -d` puts instructions at
  exactly those offsets.
- `cpu.oncpu_time` through `fs:proc` (shown): polls 940 ms, the reap 950.5
  ms, no loss.
- The context-switch rate (reasoned, unit test): `cpu.context_switches`
  needs `perf_event_paranoid` ≤ 1 here.
- The kallsyms read aside (reasoned): at paranoid 2 no profile has a
  kernel half, so the attach delay was not measured here. Wanted:
  `sudo sysctl kernel.perf_event_paranoid=1` is not this wave's; with the
  capabilities, `omn trace cpu.profile --pid <spinner> -d 2s` and the
  "attached N ms after omn started" note, against 1.0 to 1.5 s before.

## Tests

`--lib collect::cpu perf catalog validate`, `--test family_cpu --test
scenario_cpu --test cli`, clippy, fmt. `scenario_cpu`'s unimplemented-mode
test moved to the pinned `pmu` row: `mem.cache_l1 -m sample` is served now.

## Needs from another stream

- **mem**: `func`, `within`, `insn` in `mem.cache_*`'s `keys:`.

## Decision lines for `DECISIONS.md`

- A frequency-mode sample weighs the period the kernel chose for it.
- `cpu.profile`'s `pmu` row stays as a pointer to `cpu.cycles -m sample`
  (the coordinator's line).
