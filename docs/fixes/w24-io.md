# The twenty-fourth wave, stream io

Branch `feat/w24-io`. Defects first, then the additions the comparison
asked for (`docs/fixes/w22-io.md`, `docs/fixes/w23-io.md`, `STATE.md` "After
the twenty-third wave" item 1). `docs/scenarios/io.md`, "The twenty-fourth
wave", has each item as a numbered entry (25 to 29).

**No shared layer touched.** The fork-following reuses
`src/bpf/sched_common.rs`'s `Descendants` and `push_current_followed_filter`
as they are, called from io's own programs; nothing in that file changed.

## What landed

| Commit | What |
|---|---|
| `9b58868` | **fix(io)**: `io.latency` (ring and kernel histogram), `io.queue` and `io.size` follow the processes a launched program (or `--pid`) forks: the block programs take a `Descendants` map for a process target, the root compared as a constant, any other thread group looked up. New `Kept` in `src/bpf/io.rs`; `unfollowed` said as a loss note. |
| `e5f87b6` | `dir` as a key on `io.latency`, `io.queue`, `io.size` (catalog keys and a `KeyValue::dir` on every block event, from the operation byte `rwbs` reads), with a caveat line. |
| `45338b3` | **`io.util`** (new source, `fs:proc`, poll and count): `util` = io-ticks over the window, `busy_ns`, `queue` (aqu-sz) beside it. |
| `8aa4c20` | The two caveats: a swap on `zram` (on `io.latency` and `io.bytes`), a filter on a multi-device filesystem's source device. |
| `1a0c316`, `b1c4fbe` | **`io.vfs`** (new source, `bpf:fentry`, count and poll): one fentry program per VFS entry point (12, `do_fsync` among them), an atomic add each, any file type, descendants followed. One event per interval: `calls` the total, each function's count under its name, `--filter 'op == "vfs_open"'` keeps one. |
| `b1c4fbe` | **fix(io)**: `io.fsync`'s programs stay attached until the collector is dropped; the trampoline unlink no longer lands inside the window. |
| `da67a80` | **fix(io)**: `io.fsync` follows forked children. |
| `068f8ec` | **fix(io)**: `io.pagecache` and `io.readahead` follow forked children. |
| `a12f7a1` | `docs/scenarios/io.md`, defects 25-26 and entries 27-29. |

## Shown vs reasoned

All on this machine, `target/debug/omn` carrying `cap_bpf,cap_perfmon`
(`make` before each eBPF run), with fifteen other streams loading it (load
average about 30).

**Shown:**

- **Forked fio** (`thread` removed from `tests/reference/workloads/io/randread.fio`):
  `io.latency -e summary` 21 329 requests, fio `total_ios` 21 329; `-e hist`
  (kernel histogram) 21 224 for fio's 21 224. Before: 0.
- **`dir` key**: `rw=randrw` direct fio job, `--keys dev,dir`: `dir:read`
  n=16 391, `dir:write` n=16 380; fio 16 391 and 16 380.
- **`io.util`** against `iostat -d -x -y -o JSON 4 1` over the same four
  seconds of a fio job: `nvme0n1` 49.37 % vs 49.38 %, queue 17.772 vs 17.77,
  `zram0` 0 vs 0.
- **`io.vfs`**: `dd if=/dev/zero of=/dev/null bs=4k count=100000`:
  `vfs_read` 100 003, `vfs_write` 100 003; `strace -c` 100 003 read and
  100 003 write. `--filter 'op == "vfs_write"'` on `dd ... count=12345`:
  12 348 (12 345 and dd's three stderr writes). Poll timeline of 500 000 dd
  reads+writes: 544 190 + 455 839 + 2 = 1 000 031. A forked `sh -c` running
  dd twice: 1 000 011 reads, the descendants followed. An `fsync(2)`: 0 at
  `vfs_fsync_range`, 1 at `do_fsync`.
- **`io.vfs` cost**: a 1-byte read/write loop of 6 M calls, 1.36 s alone,
  1.95 s counted (three runs: 1.395/1.94, 1.374/2.004, 1.285/1.915), so the
  catalog declares `high`. Reasoned only: a call that does work dilates less.
- **`io.fsync`'s bench band**: `perf record -g` of one run:
  `__bpf_trampoline_link_prog` 13.22 % and `bpf_trampoline_unlink_prog`
  12.95 % of `omn`'s samples, both `ftrace_replace_code`. In-window cpu
  69.7 / 70.2 / 70.4 ms before (265-343 % of a 20-26 ms target), 0.99 / 1.7 /
  1.1 ms after (4.7-6.4 %).
- **`io.fsync`** under `sh -c` running two programs that each fsync: both
  syncs timed. **`io.pagecache`** under `sh -c 'cat <cold 64 MiB>'`: 16 384
  pages filled, the file exactly.
- Tests: `cargo test --lib io::` (84) and `collect::io` (65), `pagecache`
  (14), `io_fsync` (4); `cargo test --test family_io` 14 passed; `cargo test
  --test scenario_io` 43 passed, 2 ignored, 1 failed under load
  (`a_cold_read_of_a_known_file_is_measured_against_the_bytes_it_had_to_move`,
  "the session's window never enclosed the read") and passed alone at once;
  the five new scenario tests passed after (`a_launched_programs_forked_child_is_followed`,
  `every_vfs_write_of_a_forked_child_is_counted`,
  `the_syncs_of_a_forked_child_are_timed`, and the two sweeps with `io.util`
  and `io.vfs` added). `cargo test --test catalog_model` 19, `--test
  bench_bands` 6. `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo fmt --check` clean.

**Reasoned, not run:**

- The zram caveat's "no `block_rq_*` tracepoint sees it" (zram is bio-based;
  the swap here is `/dev/zram0`, 0 used, so no swap-in was forced). The
  measured gap is `docs/fixes/w23-cross.md` item 4's.
- `io.queue` and `io.size` following descendants: same code path as
  `io.latency`, not run against a forked workload.
- `a_launched_programs_forked_child_is_followed` failed once, at its first
  run, and passed the next six; the message was not kept.

## Not done, and why (ranked)

1. **A block request a `kworker` dispatched** charged to the bio's
   submitter. Not reproduced: twelve launched runs of a 4 MiB write + fsync
   and two of `view/workloads/process/workload.py` attributed 32 of 32 128 KiB
   writes to the program each time. A system-wide run shows kblockd's
   `kworker/13:1H` issuing other programs' writes, so the path exists. The
   fix (record the submitter at `block_rq_insert`, carry it to the issue) is
   a third program on the hot path and would be reasoned only. Next wave,
   with `omn top`'s 22-source `process-run` recording as the reproducer.
2. **`io.syscall` and `io.fs` following forked children**: their entry
   program is `fentry::HistogramProgram`'s in `src/bpf/fentry.rs`, a shared
   layer (see Needs). Note: stream lazy's `default:` block for io names
   `io.syscall`, so `omn trace io -- <forking program>` misses the children
   until this lands.
3. **`op` as a series key on `io.vfs`** (see Needs).
4. The runner pairs for the new numbers (`io.util` against `iostat %util`,
   `io.vfs` against `bpftrace`'s vfsstat or `strace -c`, a forked-fio pair):
   `tests/reference/` and `catalog/references/` are stream reference's. The
   numbers above are what such pairs would hold.

## Privileged runs wanted

None beyond what ran. `sudo -n perf record` (a granted witness) was used once
to profile `io.fsync`.

## Decision lines for `DECISIONS.md`

- **A process target follows the processes it starts, in every io program**
  (block, fsync, page cache, VFS calls), through `sched_common::Descendants`,
  the way the sched sources and `cpu.profile` do; `--pid` included.
- **An fentry collector unlinks its trampolines after the window**, not in
  `stop()`: the unlink walks every ftrace record and is a fixed cost of the
  tool, like the attach that already falls before the window.
- **`io.util` is a share of 1, not a percentage**, and `queue` (aqu-sz)
  travels beside it.
- **`io.vfs` is one event per interval** with each function's count beside
  the total, filterable on `op`, until a key for the function exists.

## Needs from another stream

- **coordinator** (`src/bpf/fentry.rs`, shared): `Narrowing` with a
  descendants map fd, and `entry_instructions` comparing the root then
  looking the thread group up (the shape of `io_fsync.rs`'s
  `entry_instructions` in `da67a80`), so `io.syscall`, `io.fs` and
  `net.socket` follow forked children; `HistogramProgram` holding the
  `Descendants`.
- **emit** (`src/request.rs` seam and `src/event.rs`): a `KeyKind::Op` /
  `KeyValue::Op { name }` (`op:vfs_read`), so `io.vfs` gives a series per
  function; the same key would serve `proc.syscall`'s `syscall` ask if it is
  spelt generally.
- **run** (`src/main.rs`'s cost line): the process cpu spent after a
  collection's window closed (teardown, such as the fentry unlink, about
  69 ms for `io.fsync`), said as the cpu before it opened is.
- **coordinator** (`docs/bench.md`, `tests/bench_bands.rs`): the `io.fsync`
  row's "stands on its dilation: the share is the fentry attach" is now
  wrong twice -- the share was the unlink, and it is gone from the window;
  the next backends bench should read it near 5 %, and the row's reason
  then fails the test by design. `net.tcp.connect`'s 39 % is likely the same
  unlink (stream net's `src/collect/net/connect.rs`).
- **reference**: the three pairs in "Not done" 4; the iostat `%util` row in
  `docs/replaces/io.md` can now cite `io.util`.
- **coordinator** (`CATALOG.md`, `man/omn.1`): `io.util` and `io.vfs` rows;
  `dir` among `io.latency`'s keys.
