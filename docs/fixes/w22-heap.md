# Wave 22, stream heap -- handoff

`valgrind --tool=dhat`, `valgrind --tool=massif`, `heaptrack` and `memleak`
against `mem.heap.alloc`, `mem.heap.live` and `mem.heap.retained`.

## What landed

- `catalog/references/heap.yaml`: nine pairs, one workload for all of them.
- `tests/reference/heap.rs`: four parsers (`dhat_summary`, `massif_peak`,
  `heaptrack_summary`, `memleak_outstanding`), seven unit tests on the
  captures.
- `tests/reference/workloads/heap/blocks.py`: `N` blocks of `SIZE` bytes
  from libc's `malloc` through `ctypes`, written to, the peak held `HOLD`
  seconds, all freed but `LEAK`. Run as `64 1048576 4 0.3`: 64 MiB of a
  73.6 MB total, so the interpreter's own allocations are the minority and
  vary by a few blocks between runs.
- `tests/reference/captures/heap/`: `dhat.stderr`, `massif.out`
  (`--depth=1 --detailed-freq=1000 --threshold=10`, 590 lines),
  `heaptrack_print.txt` (the header and the closing summary; the 6,896 lines
  of per-site tables between them cut) -- all three produced on this machine.
  `memleak.format.txt` is **a format fixture, not a capture**: written by
  hand from libbpf-tools memleak's format strings, with synthetic numbers
  (two reports, the last summing to 321 bytes in 6 allocations), because
  memleak needs root.
- `docs/replaces/heap.md`: seventeen rows.

## The pairs, shown vs reasoned

The runner (stream reference) was not on `main` when this stream ran, so
`cargo test --test reference` ran only the parser tests. The pairs were run
by a scratch emulation of the runner (same argv, omn's JSON read at
`<emit>.<field>` of the first series, the tool's output through the same
parsing rules), once each, on this machine, loaded by the other streams:

| Pair (tool, key) | omn | tool | rel | tolerance | verdict |
|---|---|---|---|---|---|
| dhat `total_bytes` vs `topk.total` | 73,637,410 | 73,637,410 | 0 | 0.001 | pass, shown |
| dhat `total_blocks` vs `topk.events` | 3,189 | 3,189 | 0 | 0.01 | pass, shown |
| dhat `gmax_bytes` vs `summary.max` | 68,908,064 | 68,908,064 | 0 | 0.01 | pass, shown |
| dhat `end_bytes` vs `summary.last` | 4,605,465 | 4,605,465 | 0 | 0.005 | pass, shown |
| massif `peak` vs `summary.max` | 68,908,064 | 68,908,064 | 0 | 0.01 | pass, shown |
| heaptrack `calls` vs `topk.events` | 3,189 | 3,183 | 0.0019 | 0.01 | pass, shown |
| heaptrack `peak_bytes` vs `summary.max` | 68,908,064 | 68.98M | 0.0010 | 0.01 | pass, shown |
| heaptrack `leaked_bytes` vs `summary.last` | 4,605,465 | 4.61M | 0.0010 | 0.01 | pass, shown |
| memleak `bytes` vs `summary.last` | -- | -- | -- | 0.01 | **skips**: `privileged: true`, `sudo -n` not granted; reasoned only |

What the real runner will do with them is reasoned, not shown: it has to
resolve `topk.total`/`topk.events`/`summary.max`/`summary.last` and append
the workload's argv to the `sh -c` commands (heaptrack, memleak) as `$@`.

Found on the way, shown:

- dhat's `At t-end` matches only with `--run-libc-freeres=no`: valgrind
  otherwise frees glibc's own buffers at exit (12,179 bytes here), which a
  native run never does.
- `heaptrack` without `--record-only` launches `heaptrack_gui` at the end
  and blocks a test forever wherever a display is reachable. The pair passes
  it.
- heaptrack reads 72 KB higher at the peak than dhat, massif and omn (which
  agree to the byte); its own in-process allocations are the likely reason
  (reasoned).
- The BRIEF's list of tools not installed is stale for this family:
  `heaptrack` 1.5.0 and libbpf-tools' `memleak` (`bcc-libbpf-tools` 0.37.0)
  are installed here.

## Gaps in omn (each a line of work for the waves after)

1. **`mem.heap.alloc` serves no stack.** The source is "allocations by call
   site" and lists `stack` in `emits`, but the preload backend captures none:
   `omn trace mem.heap.alloc -e stack --format folded -- prog` runs the
   program to the end, then fails with "`stack` was asked for and no event
   carried one, 3186 of them; the backend that served this request captured
   none" (with `--stack fp` and `--stack dwarf` alike). That is the
   invariant "an invalid combination is refused with the reason, never
   degraded silently" broken in time: the refusal comes after the run. And
   it is what every one of these four tools is mostly used for. Wanted:
   stacks from the shim (frame pointers at least), or `stack` dropped from
   the backend's emits so the resolver refuses up front.
2. **No eBPF collector for `mem.heap.alloc`**: `--backend force:bpf:uprobe`
   and `force:bpf:usdt` both answer "no collector is written ... yet". So no
   attach to a running program (`heaptrack -p`, `memleak -p`), which the
   preload shim cannot do by construction.
3. **`mem.heap.live` has no high-water mark.** It is polled, so a peak
   shorter than the period is missed: without the workload's hold, 60.5 MB
   of a 68.9 MB peak at `poll@1ms` (which delivered 6 samples in a 50 ms run,
   so the period floor is well above 1 ms). dhat, massif and heaptrack all
   report the exact peak. The shim sees every call; it could keep the
   maximum.
4. **`summary` has no sum.** `count`, `min`, `max`, `mean`, `last` only;
   the total bytes allocated is read from `topk:1`'s `total` (or
   `count * mean`).
5. **Live blocks are not counted**, only bytes (dhat's `At t-end: ... in 52
   blocks`, memleak's allocations).
6. **No allocation lifetime**: heaptrack's temporary allocations and dhat's
   block lifetimes need a free paired with its allocation (the free events
   carry `ptr` and `freed_bytes`, the pairing is not exposed).
7. Out of reach of an interposer, filed for completeness: dhat's reads and
   writes per block.

## Not done, and why

- The memleak pair has never run: root. It has no capture, only a format
  fixture, so its parser is checked against the format, not the tool, and
  the pair is not evidence until the privileged run below.
- No pair for massif's timeline: its time axis is instructions executed,
  omn's the monotonic clock.
- No pair for `mem.heap.retained`: no tool here measures freed pages kept by
  the allocator. Shown on the workload: 131,072 bytes, as expected, glibc
  having unmapped the `mmap`ed 1 MiB blocks.
- No same-execution pair: valgrind replaces malloc on its own CPU, and omn's
  shim and heaptrack's are both `LD_PRELOAD` interposers. Every pair says
  "alternate runs" in its `why`, and the workload is deterministic to the
  block, which is what makes that sound.

## Privileged runs wanted

The memleak pair, once the user grants sudo for it or runs the test as root:

```console
sudo memleak -T 100000 -o 0 -c "python3 tests/reference/workloads/heap/blocks.py 64 1048576 4 0.3" 2 1
```

Then save that output as `tests/reference/captures/heap/memleak.txt`, add a
test reading it (keep `memleak.format.txt`'s, it covers the multi-report
case), and check the
pair passes: `cargo test --test reference` with `sudo -n` granted for
`memleak`. Whether libbpf-tools' memleak exits when the `-c` command does,
and still reports its outstanding allocations after, is reasoned, not shown.

## Decision lines

- The heap pairs compare totals only; per-site comparisons wait for gap 1.
- `omn:` paths are `<emit>.<field>` of the first series of that emit
  (`topk.total`, `summary.max`): the runner should support it.
- Valgrind runs with `--run-libc-freeres=no` in every pair.
- heaptrack runs with `--record-only`, always.

## Needs from another stream

- **reference**: the runner must (a) resolve `omn:` as `<emit>.<field>` of
  the first series of the output with that `emit` (this family uses `topk`
  and `summary`), (b) append the workload's argv after a `sh -c SCRIPT NAME`
  command so the script sees it as `$@`, (c) check `requires` before
  `privileged`. Its `common.rs` helpers (`number`, `capture`) were not on
  `main` when this ran; `heap.rs` carries its own small ones and can switch.
- **bpftrace**: `memleak` is in both rows; this family's pair compares it
  against `mem.heap.live`, the bpftrace stream's can keep the kernel mode
  (no `-p`) against `mem.kmalloc`.
- **src (next wave)**: gaps 1 to 6 above.

## Review

- **The memleak capture was not memleak output** (gap, reviewer right).
  `memleak.documented.txt` held two stacks whose sums (4,194,304 B in 4 plus
  411,161 B in 48) were chosen to equal dhat's `At t-end` (4,605,465 B in 52
  blocks), so its test checked the parser against dhat's numbers dressed as
  memleak's. Fixed as far as this session can: the file is renamed
  `memleak.format.txt`, its numbers replaced by synthetic ones that agree
  with no tool (two reports; the last sums to 321 B in 6 allocations, the
  first's 1,000 B must be ignored), and the test says it is a format
  fixture. The privileged capture itself is **still not done**: `sudo -n`
  is not granted for memleak here (`sudo: a password is required`). It
  stays under "Privileged runs wanted", and the pair counts as reasoned
  only until `memleak.txt` exists.
