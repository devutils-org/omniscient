# Stream views, the twenty-fifth wave

Branch `feat/w25-views`. Done sequentially by one agent, in the main
checkout.

## Outside the owned files, said first

- `docs/view/process.md`, `docs/view/README.md` (docs'): the loss badge as
  it is now drawn (`79 lost`, `12 lost (3.1%)`).
- `docs/view/gallery/*`: snapshots rewritten under `OMN_VIEW_BLESS=1` for
  the two drawing changes below (the ramp's colours, the badge's sign); the
  characters of every other snapshot are unchanged.

## What landed

1. **`usage::RAMP` reads at 3:1 on a light terminal**: `28, 64, 100, 136,
   166, 160`, the ramp `tests/view_gradient.rs` proposed; its light-terminal
   test un-ignored and passing; the gradient strips redrawn.
2. **The `Lifetime` mirror's `exit_code` and `signal`**, and
   `Lifetime::ending` (`exited 3`, `killed by signal 11`, `stopped by the
   run`), said in the `process` header after the run's length.
3. **`ON LOCKS -58 lost` read as a negative count**: a loss badge is
   `58 lost` now, and the heatmap's strip words (`10 dropped`, `56 let go`)
   lost their minus signs too.
4. **The `process` suspect reading sleep where `sched.futex` holds the
   wait**: a decomposition row already read as `blocked: sleeping` gives its
   futex waits to a lock part, as a split row's sleep did; the verdict then
   names the lock.
5. **Flows' `dir:` fold**: a machine source keyed on `dir` folds its
   direction series into one sink per device, both directions' bytes.

## Shown vs reasoned

All by tests: the contrast test on four backgrounds; the mirror's round trip
and `ending`; the heatmap and request texts; a sleep row three quarters on
futexes named a lock; a recording's `net.bytes` split by direction drawing
the same sinks and total.

## Not done

1. **Flows' file -> device ribbon: settled, not drawn.** The junction is
   already two layers: the `cache` column carries the files' volume (115 MiB
   in the gallery's run) and the device column the device's (15.3 MiB), each
   at its own height. A ribbon from each file to a device would need each
   file's device bytes, which nothing measures -- the page cache merges
   them, and writeback writes them later from a kworker. The textual edge
   (`◂ N files`, from `io.fs`'s `blk_devs`) is what is known, and stays.
2. **A placed drop on screen**: needs a recording whose drop is on one of
   the heatmap's four sources (`sched.runqlat`, `io.latency`, `net.socket`,
   `gpu.kernel`), which needs a load that overruns a ring under CAP_BPF.

## Tests

`-p omn-view --lib`, `--test view_gradient --test view_snapshots --test
view_model --test view_timebar --test view_truth --test view_top_batch`,
clippy, fmt.
