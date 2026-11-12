# Brief — closing the lists

The thirty-second wave. Run on 2026-11-12 after the user's *"go essayer de
tous boucler"*: take every item `docs/dev/STATE.md`'s "What remains" listed
after the twenty-ninth and thirty-first waves to its end where this machine
or a distroproof guest can, and write down why where it cannot. One worktree
(`omniscient-w32`, branch `claude/w32`), one item after another.

## What it did

1. **The view tests that went red with the TUI's `overview`**: a view that
   asks no source has no workload to be held to (`tests/view_workloads.rs`).
2. **`/proc/config.gz` without `zcat`**: the member's header by hand, the
   stream through `miniz_oxide`, held to what `zcat` reads.
3. **`tcp.unsent-backlog`**: a connection holding over 1 MiB unsent, no low
   mark; proved under `unshare -rn` (2.49 MB -> 0.20 MB unsent, 225 -> 32 ms
   from write to read). `net.tcp.rtt` takes a launched program.
4. **`vm.swappiness`**: measured in guests, three fixtures, and refused --
   its prediction is not one-sided.
5. **`irq.on-the-workload-cpus`**: interrupts on a pinned workload's cpus,
   proved in guests (0.42-0.47 of the run -> 0.0003; the spin 889 -> 386 ms).
   The rules gained `on: target_cpus` and `per: {run: wall_ns}`.
6. **`runtime.func`'s histogram in the kernel**: `-e hist` alone counts into
   rows (`fentry::RowMaps`, `count_into_rows`, taken out of the
   `io.syscall` histogram); on two million calls, none lost against 298 542.
7. **Phase 4's `dm-delay`**: the vertical server's file on a delayed device,
   in guests (`distroproof-vertical.toml`): 321.5 and 315.7 ms in
   `block_io`, held in the block-io table's residue.

## What it found

- The release did not link: `stack::mapped_identity_of` called glibc's
  `statx` (2.28) in an archive linked against 2.17.
- A stripped file's origin dropped debuginfod's answer; the sqlite cases now
  skip with the server's 404 instead of failing.
- A launched program that ends before omn's first read has no process
  setting (filed); the THP fixture waits before its faults.
- A ratio with a lost numerator was read as no bound.
- The `dlopen` scenario's bound was tighter than the rescan allows.

What is left is `docs/dev/STATE.md`'s "After the thirty-second wave".
