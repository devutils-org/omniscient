# The twenty-third wave, stream cross

`tests/cross_machine.rs`,
`a_memory_limit_turns_a_cached_read_into_device_reads_and_each_byte_carries_its_rung`:
the two filings against it in `docs/open-findings.md` ("the writeback lands
after the program's window", twenty-second wave; "reclaimed pages outside the
program's intervals", twenty-first wave) were read, and neither cause was the
one filed. Four separate defects of the test, all fixed in the test, nothing
under `src/`.

## What landed

Commits on `feat/w23-cross`: `5bcf37c`, `4e38445`, and the documentation commit
after them; the files are `tests/cross_machine.rs` and
`examples/cross-machine-reclaim-io.yml` (comment only).

1. **The TEMPORAL rung's 18.3 and 32.7 MiB: a second device, not late
   writeback.** `/var/tmp` here is a btrfs volume of two partitions,
   `nvme0n1p6` and `nvme0n1p9` (`/sys/fs/btrfs/<fsid>/devices`). The mount's
   source names only `nvme0n1p6`, and the test counted only the writes on that
   one. Btrfs puts each chunk of the file on either member. In one dumped run,
   32.5 MiB of data writeback went to `p6` and 151.2 MiB to `p9`, and the
   program's own reads came from `p9`'s sectors. How much landed on `p6`
   depends on which chunk the allocator picked, not on load. The program's
   cgroup held **0.0 MiB dirty and 0.0 MiB under writeback** at its last read
   in every capped run measured, quiet or loaded, so the writeback was never
   after the window. Before the fix, the writes from other processes on `p6`
   (other streams' builds: 100 to 450 MiB per window) hid the gap. A quiet
   run with the program launched showed 39.9 MiB. **Fix:** `Scratch` now holds
   every member of the filesystem (`members()`, read from
   `/sys/fs/btrfs/*/devices`, or the device alone) and every disk they sit on.
   The TEMPORAL rung and `io.bytes` read all of them.
2. **The window: launched by the session, ended at the drained writeback.**
   The run was a fixed `duration: 3s` with the program started 700 ms after
   `omn`. Under load the program outran it: "the device poller's ticks do not
   bracket the program" in 2 of 4 loaded runs, one program taking 6.6 s.
   **Fix:** the test adds one request to the shipped session, `mem.rss` on
   `target: {launch: [sh, -c, 'echo $$ > <cgroup>/cgroup.procs && exec
   python3 ...']}`. The run's gate now holds the program until every
   collector is attached: the trace opened 16 to 27 ms before the first
   write, against a head start that was a guess. The program's exit ends the
   run (`-d 60s` is only a ceiling). When capped, the program polls its
   cgroup's `memory.stat` until `file_dirty + file_writeback` is 0 (at most
   20 s) and reports that instant. It then sleeps 300 ms so that
   `mem.reclaim`'s last 100 ms window and the poller's next tick fall inside
   the run. The containment check ends at `settled_ns()`, the later of the
   last read and the drain; the TEMPORAL rung's bound ends at the last read
   (Review, below). Containment now
   uses each poll's `interval_ns`: the first tick reports since the baseline
   read when the window opened, and that baseline now comes before the
   program. Side effect: the test takes 2.5 s instead of 7.
3. **The stray reclaimed pages: the program's own, after its last read.**
   Reproduced under load: *"256 of the 75569 pages credited lie in intervals
   the program did not overlap (0 before its first write, 256 after its last
   read), and only 0 of the pages the tool saw reclaimed were another
   cgroup's"*. They sit on the **late edge**, and they are this cgroup's. In
   that run the cgroup swapped in 36 398 pages. After `t2` the program reads
   `/proc/self/io` and `memory.stat`, waits for the writeback and unlinks the
   file, so it faults and reclaims after its last read. **Fix:** the program
   reports `done` (after all of that, just before it prints), and the
   brackets are first write to `done`. The assertion is now precise: pages
   credited outside the brackets must be at most the pages the tool saw
   reclaimed under *another* cgroup. The old form passed whenever the
   machine-wide total was more than 5 % + 1024 pages over the cgroup's.
4. **IDENTITY under load: swap-ins.** Found while testing under load:
   *"the kernel charged the program 108.0 MiB of storage reads and the block
   layer's submitter was the program for 96.0 MiB"*, three times in five.
   `/proc/self/io`'s `read_bytes` counts swap-ins. The swap here is `zram0`,
   a bio-based device, so neither `io.latency` nor `io.bytes` on `nvme0n1`
   sees those reads. Measured: 2 217 pages swapped in, 8.7 MiB, against a gap
   of 8.3 MiB. **Fix:** the program reports what its cgroup's `pswpin` grew
   by over its run and subtracts it, times the page size, from `read_bytes`. First tried: setting
   `memory.swap.max` to 0. That OOM-killed the program in 2 of 6 loaded runs
   (`memory.events` `oom_kill 1`): its anonymous pages had nowhere to go while
   every file page was dirty or under writeback. Reverted. The swap-in count
   is printed on the ladder's `program` line.

Diagnostics added to the ladder (`edges` line): when the trace attached
relative to the first write, the cgroup's dirty and under-writeback bytes at
the last read and when they drained, bytes written to the filesystem's devices
after that, and pages laid before the first write and after `done`. A program
that does not report now says how the run stopped and the cgroup's `oom_kill`
count. Before, that information was lost in six lines of notes and caveats.

## Shown vs reasoned

Shown, on this machine, `make` run first (capabilities granted):

- **Alone, five in a row: 5 of 5 green** (load average 10, from the other
  streams), TEMPORAL 97 to 213 MiB, 0 pages outside the brackets.
- **Under load: 6 of 6 green.** The load: one shell busy loop per cpu plus
  four `dd ... conv=fsync` writers of 256 MiB on `/var/tmp`, in a loop, at
  load average 23 to 34. `stress-ng` is not installed. Swap-ins in those
  runs: 0 to 18 435 pages.
- **Beside a parallel `cargo build --workspace --release`: 3 of 3 green.**
- The whole file, `cargo test --test cross_machine`: 6 of 6, twice.
- Defects 1, 3 and 4 are each shown by a measured run quoted above. Defect 2
  by the containment failures before the fix and none after.

Reasoned, not shown:

- That the twenty-second wave's two red runs under the full suite were defect
  1. The full suite was not run (the brief: the coordinator's, at the end). It
  is the only cause found that gives tens of MiB short with zero dirty bytes
  left, and it does not depend on load. The coordinator's run is the check.
- That the twenty-first wave's 64 and 72 pages were the late edge (defect 3):
  the same signature, but those runs' documents are gone.
- That the cgroup's own reclaim cannot lie outside first write to `done`:
  argued in the comment above the assertion (an episode or a drain window
  that began inside the brackets lies in an interval that overlaps them).

## Not done, and why

- **`a_drained_reclaim_row_is_credited_whole_inside_one_second_of_pressure`**
  shares `run_reclaim_of` and gets the launched window and the `done`
  brackets. It passed in both whole-file runs after the change, but it was not
  run five times or under load: outside the brief.
- The flaky tally entries for this test in `docs/open-findings.md` are not
  edited: not this stream's file (below).

## Privileged runs wanted

None. Everything ran unprivileged, with `make`'s capabilities.

## Decision lines

- A cross-machine session whose window must contain a program launches that
  program from the session, and the program decides when it is done.
  Neither a duration nor a head start is reliable under load.
- A reference read from `/proc/<pid>/io` is net of the cgroup's swap-ins
  over the same run (a delta of `pswpin`, never its cumulative value)
  whenever it is compared with block-layer counts.
- A filesystem's devices are its members (`/sys/fs/btrfs/*/devices`), not
  its mount's source.

## Review

Both findings were right; both are fixed in `tests/cross_machine.rs`.

1. **`pswpin` was cumulative.** The two runs share one cgroup, so the
   uncapped run subtracted the capped run's swap-ins from its own
   `read_bytes`. A negative result then parsed to 0, which masked the
   "short of memory uncapped" skip and tightened the uncapped IDENTITY check
   for no reason. **Fix:** the program reads `pswpin` before its first write
   and reports the delta. Every field of its report is now parsed strictly:
   one that is missing, negative or not a number panics with the report line
   instead of reading as 0. Measured after the fix: capped 1 012 pages,
   uncapped 0 (before, the uncapped line repeated the capped count).
2. **TEMPORAL's 64 MiB bound had lost its meaning.** Since the program waits
   for its cgroup's writeback to drain, every one of the 96 MiB was written
   back by `settled_ns()` whatever the limit did. **Fix:** three checks in
   place of one. (a) The limit's bound, at least 64 MiB less 10 %, now counts
   only the writes from the first write to the **last read** (`end_ns`), the
   new `writes_by_end` on the ladder. (b) The mechanism is asserted: at the
   last read, `dirty_at_end + writeback_at_end` is within the 32 MiB limit.
   (c) The capture, reworded: by the drain, at least 96 MiB less 10 % reached
   the file's devices. The ladder's TEMPORAL line prints `writes_by_end`.

Shown: `cargo test --test cross_machine`, 6 of 6, twice, plus the reclaim
test alone with `--nocapture`, after `make`. The machine was not loaded on
purpose. In those runs TEMPORAL saw 756 MiB by the last read: other writers
on the device pass (a) and (c) as easily as the program does. The block layer
does not tell them apart, which is why this rung is TEMPORAL.

## Needs from another stream

### To the coordinator, `docs/open-findings.md`

Close both sections: "`cross_machine::a_memory_limit_...`: the writeback lands
after the program's window" (the cause was the btrfs volume's second device,
above, item 1) and "... reclaimed pages outside the program's intervals" (the
late edge, item 3). Don't add this test to the flaky tally unless the
end-of-wave suite turns it red again. If it does, the `edges` line in its
output says which edge.

### To `catalog/io.yaml` (owner of `io.latency` and `io.bytes`), for a later wave

Two caveats this crossing found, as candidate text, not filed as defects:
a swap on a bio-based device (zram) is read I/O that `/proc/<pid>/io` charges
and neither source sees; and on a multi-device filesystem one file's I/O lands
on several partitions, so a filter on the mount's source undercounts it.
