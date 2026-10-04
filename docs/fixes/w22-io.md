# Wave 22, stream io: handoff

`fio`'s own counts and latency, and `iostat`'s columns, against `io.*`.

## What landed

- `catalog/references/io.yaml`: seven pairs. Four against `fio` (reads
  completed and mean completion latency through `io.latency` on a launched
  fio; bytes and operations through `io.bytes` and `io.iops` machine-wide
  alongside it), three against `iostat -dx` (`r/s`, `rkB/s`, `r_await`).
  The header documents the three fields the pairs add: `same_run`
  (`omn_launches_tool` or `alongside`), `duration`/`start_after`, and the
  `compare.omn` path spelling `<emit>[<key glob>].<field>`.
- `tests/reference/io.rs`: `fio_json` (fio's JSON report, per direction)
  and `iostat_json` (`iostat -d -x -o JSON`, per disk and summed into
  `all.*`, kB/s also in bytes, awaits also in ns), five unit tests.
- `tests/reference/captures/io/fio-randread.json`: a real fio report from
  this machine. `tests/reference/captures/io/iostat-dx.json`: **written by
  hand** in sysstat 12's JSON layout, with values from this machine's fio
  run, because `iostat` is not installed; replace it with a real capture
  once sysstat is.
- `tests/reference/workloads/io/randread.fio`: the job file (4 KiB random
  direct reads, psync, `thread`, 2 s, file under `target/`).
- `docs/replaces/io.md`: twelve rows, `fio` and each `iostat -dx` column.

## Shown vs reasoned

Shown, by hand on this machine, with `target/debug/omn` carrying
`cap_bpf,cap_perfmon` (the runner of stream reference was not on `main`, so
no pair ran through it):

- Pair 1, `io.latency` count vs fio `total_ios`, same run: equal three times
  (29335/29335, 28837/28837, 29118/29118); +1.9 % on the layout run.
- Pair 2, `io.latency` mean vs fio `clat` mean, same run: 55.9 us against
  68.0 us, omn 18 % below, within the declared `rel 0.25`.
- Pair 3, `io.bytes --system -k dir` vs fio `io_bytes`: 121532416 equal.
- Pair 4, `io.iops --system -k dir` vs fio `total_ios`: 29671 equal.
- System-wide `io.latency` count vs fio `total_ios`: 14514 equal (not kept
  as a pair; pair 1 is the same execution).

Reasoned, not run: pairs 5 to 7 (`iostat` missing; they skip on
`requires: [iostat, fio]`), and every tolerance's behaviour under the wave's load.
The `iostat` parser is tested only on the hand-written capture.

## Tests

`cargo test --test reference`: 5 passed (the io parser tests; the runner
has no pairs to run on `main` yet). `cargo test --test cli`: 28 passed.
`cargo clippy --workspace --all-targets -- -D warnings`: clean.
`cargo fmt --check`: clean. A system upgrade (pacman, 19:36-19:37) left
`rustc` unable to load `libLLVM.so.22.1` for a few minutes mid-stream; the
checks above were rerun after it completed, on rust 1.99.0.

Fix round, after merging `main` (9d367ef), touching only the YAML and this
handoff: `cargo test --test reference` 23 passed (every family's parser
tests; still no runner), `cargo test --test cli` 28 passed, clippy and
`cargo fmt --check` clean. The YAML was also loaded with a YAML parser:
seven pairs.

## Not done, and why

- No pair for `iostat`'s merges, `aqu-sz`, `rareq-sz`, `d/s`, `f/s`: the
  workload merges, discards and flushes nothing; a sequential buffered
  workload is the next pair. `%util` has no source at all (gap below).
- No pair against fio's `disk_util`: fio does not map a btrfs file to its
  device and prints `null` here.
- Percentile pairs (fio p50/p99 against `io.latency -e hist`): left out for
  time; the parser already returns `read.clat_p50_ns` and `read.clat_p99_ns`.

## Gaps in omn (each a line of work, not fixed: `src/` is not this stream's)

1. **A launched program's forked children are not followed by
   `io.latency`.** `omn trace io.latency -m trace -e summary -- fio <job>`
   without `thread` (fio's default forks the job) reports 0 requests and
   "29678 block_rq_complete events had no opening this run recorded",
   for 28807 reads fio completed; with `thread` it reports 28453 for 28453.
   The launch target's descendants should be in the submitter filter, as
   they are in the overhead accounting.
2. **`io.bytes` and `io.iops` have no per-process collector on this build**:
   only `fs:proc` is served, so a launch or `--pid` is refused ("cannot
   attribute per launch"). The catalog lists `bpf:raw_tp`/`bpf:tp` for both;
   `omn list` names collectors for `io.size` and `io.latency` on the same
   tracepoint, not for these two.
3. **`--system` cannot be given with a program to launch** ("the argument
   '--system' cannot be used with '[PROGRAM]...'"): measuring the machine
   for the length of one program needs a duration guessed in advance, which
   is why pairs 3 to 7 need `same_run: alongside`.
4. **No `%util`**: the io-ticks column of `/proc/diskstats` over a window
   has no source; `io.inflight` gives the depth, not the busy share.
5. `summary` over `fs:proc` keyed on `dir` alone folds one event per device
   into a series (`count` 4 for 4 devices, `mean` a quarter of the bytes):
   the total is `mean * count`. The `rate` emit's `total` is the number to
   read, which the pairs do.

## Privileged runs wanted

None for the pairs as written; the eBPF pairs need only the file
capabilities `make` grants. To run pairs 5 to 7:

```console
sudo pacman -S sysstat
cargo test --test reference
```

and replace `tests/reference/captures/io/iostat-dx.json` with
`iostat -d -x -y -o JSON 2 1` taken while
`fio --output=/dev/null tests/reference/workloads/io/randread.fio` runs.

## Decision lines

- io pairs that need the whole machine run **alongside**, with a band sized
  for the other readers of the machine (which only add); the band is the
  schema's symmetric `{rel: N}`, so it also passes omn that far *below* the
  tool, until a one-sided tolerance exists. The same-process pairs are held
  to the same execution, omn launching fio.
- The fio workload runs with `thread` until gap 1 is closed, and says why in
  the job file.
- The workload file lives under `target/`, never `/tmp` (tmpfs here).

## Needs from another stream

- **reference**: the runner to honour `same_run: omn_launches_tool` (run
  `omn <omn...> --program-stderr <file> -- <reference.command...>
  <workload...>`, parse `<file>` as the tool's stderr) and `same_run:
  alongside` with `duration`/`start_after` (omn `--system -d <duration>` in
  the background, the reference command started `start_after` into it, with
  `workload`, when non-empty, run beside it); and the `compare.omn` path
  spelling `<emit>[<key glob>].<field>` with count/total summed and mean
  weighted by count over the matching series. If the runner settles another
  spelling, the seven pairs are mechanical to rewrite.
- **reference**: a one-sided tolerance, e.g. `{rel: 0.05, side: omn_above}`
  (omn may exceed the tool by the band, never fall short) and its mirror
  `omn_below`. Pairs 3 and 4 want `omn_above` (other readers only add;
  omn short of fio is lost requests), pair 2 wants `omn_below` (omn's span
  is inside fio's clat). Until then the three bands are symmetric and their
  `why` says so; the field is not written into the pairs now, because a
  runner that parses `tolerance` strictly would refuse it and one that
  ignores it would claim a check it does not make.
- **sysstat**: `iostat` is shared with that stream's row; if it writes its
  own `iostat` parser, one of the two should go. Mine is `iostat_json`.

## Review

The fix round answered the three findings of the wave's review.

1. **The tolerances are symmetric, the text said one-sided.** Fixed in the
   text: the `why` of pairs 2, 3 and 4 and the decision line now say the
   band is `{rel: N}` either way and which side it would wrongly pass; the
   YAML header says the same for every pair. The one-sided tolerance is
   asked of stream reference under "Needs from another stream", with the
   side each pair wants. Not added to the pairs themselves, for the reason
   given there.
2. **Pairs 5 to 7 run fio as their workload and required only `iostat`.**
   Fixed: `requires: [iostat, fio]`, so a machine with sysstat and without
   fio skips with the missing binary rather than failing the workload.
3. **Every pair depends on runner fields not on `main`.** Right, and not
   fixable in this stream: after merging `main` (9d367ef) there is still no
   runner in `tests/reference.rs` -- it lists the parsers and runs none.
   Nothing here can make the pairs produce a value without it. What the
   merge of stream reference must do, one of:
   - honour `same_run: omn_launches_tool` / `alongside`, `duration`,
     `start_after` and the `<emit>[<glob>].<field>` path as specified in the
     YAML header and under "Needs from another stream"; or
   - if it settles another spelling, rewrite the seven pairs to it (they are
     mechanical); or
   - if it can do neither this wave, mark the io pairs pending rather than
     run them under fields it ignores: pairs 1-2 would then run fio with
     `--output=/dev/stderr` and no `--program-stderr` file, and pairs 3-4
     with an empty workload and the job file in the tool command, and
     neither yields a comparable value. A pair that runs and compares
     nothing must fail loudly, not pass.
   None of the seven pairs has run through a runner; "Shown vs reasoned"
   already says so and still does.
