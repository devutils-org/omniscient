# The twenty-third wave, stream bpftrace

The ten bpftrace pairs rewritten to the shared runner's schema and run by
it. bpftrace needs root and `sudo -n` grants none here, so every pair skips
with the machine's reason; what is shown is `omn`'s side, on its real
output, through the same path and parser code the runner uses.

Branch `feat/w23-bpftrace`: `718365e` (the pairs, the parser, the captures,
the table) and this handoff.

## What landed

- `catalog/references/bpftrace.yaml`: ten pairs, each with an `id` (the
  tool's name without `.bt`). The family grammar (`summary.count`,
  `summary.last`, `hist.count`, `hist.p50`) is gone:
  - counts are paths, `results.0.outputs[emit=summary].series.*.count` (or
    `.last` for killsnoop's count-mode counter), summing every series;
  - bitesize compares one bucket, `[4K, 8K)`, by path:
    `results.0.outputs[emit=hist].series.*.buckets[low=4096].count` against
    the tool's `@[omnref]/4096`, instead of the whole histogram's count
    (below, "Decision lines");
  - the two medians (biolatency, runqlat) are `omn_parse: omn_hist_usecs`
    with `tolerance: {log2_buckets: 1}`, replacing `rel: 1.1`, which bounded
    nothing below the reference;
  - execsnoop and tcplife are `run: alongside` (`settle_ms: 2000`), one run:
    omn `--system -d 6s`, the tool as `timeout --preserve-status -s INT 6
    bpftrace <tool>.bt` (END maps printed on SIGINT, exit 0 passed on), the
    workload launched by the runner after the settle. The old
    `alongside: true` field is gone.
  - the eight others stay `separate` (bpftrace launches its own `-c` child
    through `under.sh`).
- `tests/reference/bpftrace.rs`: the parser `omn_hist_usecs` (every `hist`
  output and series merged by bucket lower bound; `@usecs/count`,
  `@usecs/p50`, `@usecs/<low>` in ns, the keys `bpftrace_hist_usecs` gives
  the tool's map), and three tests: the parser on two captures, its refusal
  of an output with no histogram, and
  `every_omn_side_resolves_on_its_capture`, which loads the family through
  `runner::load` and evaluates every compare's omn side (path through
  `runner::eval_path`, or parser) on the pair's captured omn output against
  the number expected.
- `tests/reference/captures/bpftrace/omn/<id>.json`: ten real `omn` outputs,
  each produced on this machine with the pair's own argv and workload
  (alongside ones with the workload started 2 s into a 6 s system run).
- `docs/replaces/bpftrace.md`: every pair row cites its id and the runner's
  verdict (skipped, root), with omn's number today; the `memleak` row now
  says `no pair:` (stream heap's), which `every_row_names_a_pair_or_why_none`
  required.
- No harness to delete: this family never had one of its own.

## Each pair's outcome

`cargo test --test reference pairs_bpftrace -- --nocapture`:
`tally: reference bpftrace: 0 held, 10 skipped, 0 gaps, 0 known defects, 0 failed`.

| Pair | Outcome | omn's side today (capture) | the workload fixes |
|---|---|---|---|
| `syscount` | skipped: `sudo -n` does not grant root | 21216 calls | 20000 getppid + startup |
| `opensnoop` | skipped, same | 623 openat | 500 + startup |
| `killsnoop` | skipped, same | 50 | 50 |
| `execsnoop` | skipped, same (alongside) | 20 | 20 |
| `bitesize` | skipped, same | 504 in [4K, 8K) | 500 + setup |
| `biosnoop` | skipped, same | 508 completed | 500 + setup |
| `biolatency` | skipped, same | median in [32, 64) us, 508 requests | -- |
| `runqlat` | skipped, same | median in [64, 128) us, 26406 waits | -- |
| `tcpconnect` | skipped, same | 10 | 10 |
| `tcplife` | skipped, same (alongside) | 20 sockets | 20 |

## Shown vs reasoned

- **Shown**: the runner loads all ten pairs, `every_pair_is_well_formed`
  names none of them (it is red on other families), and `pairs_bpftrace`
  skips all ten with the `sudo -n` line and prints the tally.
- **Shown**: every compare's `omn` side resolves on a real omn output with
  the pair's own argv, by the runner's own `eval_path` and by the family's
  parser (`every_omn_side_resolves_on_its_capture`).
- **Shown, a change since the twenty-second wave** (omn's side, not
  compared): `io.size` on `read 500` now sees 1128 requests by `omnref`,
  622 of them in [16K, 32K) (python3's own reads, cold or cached depending
  on what ran before), where it saw 508; the 4K bucket is 504 as before.
  `sched.runqlat` under two spinners per CPU (24 CPUs) gave a median in
  [64, 128) us and 26406 waits, where the earlier run gave [2, 4) ms and
  16747: the median depends on the machine's load, and the pair compares
  two separate runs (see "Not done").
- **Reasoned, not run**: every tool-side number (bpftrace needs root); that
  `timeout --preserve-status -s INT` makes bpftrace print its END maps and
  exit 0 under `sudo -n`; that 2 s is enough settle for bpftrace to attach;
  that `log2_buckets: 1` holds across two separate runs for the medians.
  The tool captures remain the `*.format.txt` fixtures written from each
  script's `printf` formats (synthetic numbers), since no tool ran.

## Not done, and why

- **No tool-side capture replaced**: none of these tools runs here without
  root (the brief's list of now-installed witnesses does not change that).
- **The medians are two runs** (`separate`): biolatency and runqlat could be
  `alongside` (both system-wide, the workload launched beside them), which
  would remove the two-run spread the runqlat numbers above show. Not
  changed without a privileged run to show `log2_buckets: 1` holds either
  way; it is the first thing to try if the privileged run misses by a
  bucket.
- **No bcc pair**: bcc tools are now installed per the brief, but every bcc
  tool needs root as well, and no capture of their output exists to test a
  parser on.

## Privileged runs wanted

```console
# from the worktree (or main once merged), by a person who can grant sudo:
make
sudo -v
cargo test --test reference pairs_bpftrace -- --nocapture
# one pair at a time:
OMN_REFERENCE_PAIR=runqlat cargo test --test reference pairs_bpftrace -- --nocapture
# real tool captures beside the format fixtures (repo root):
W=tests/reference/workloads/bpftrace; C=tests/reference/captures/bpftrace
for t in syscount:"getppid 20000" opensnoop:"open 500" killsnoop:"signal 50" \
         tcpconnect:"connect 10" bitesize:"read 500" biosnoop:"read 500" \
         biolatency:"read 500" runqlat:"spin 2"; do
  sudo sh $W/under.sh /usr/share/bpftrace/tools/${t%%:*}.bt -- \
    $PWD/$W/omnref $PWD/$W/act.py ${t#*:} > $C/${t%%:*}.txt
done
```

Then move the parser tests' numbers from `<tool>.format.txt` to `<tool>.txt`.

## Decision lines

- **bitesize compares the [4K, 8K) bucket, not the histogram's count**: the
  count now includes python3's own reads (622 at 16K here), which the page
  cache serves or not depending on the previous run, so two runs do not fix
  it; the 500 O_DIRECT reads are all in the 4K bucket on both sides.
- **`log2_buckets: 1` for the medians** is exactly the offset of the two
  grids (bpftrace's us buckets scaled to ns sit at 1000 * 2^k, floor log2
  k + 9; omn's same latency lands in k + 9 or k + 10). The earlier `rel:
  1.1` admitted one more bucket above and anything below; this is tighter
  and symmetric. If the privileged run misses by one more bucket, the fix is
  `alongside` (one run), not a wider tolerance.
- **One parser of omn's output, for the medians only**: every other value
  is a path, so the runner's own grammar reads it.
- **omn's captures are tested through the runner's `load` and
  `eval_path`**, so a path that stops resolving after an omn change fails a
  test that runs without root.

## Needs from another stream

- **Coordinator**: the privileged run above, once; it is the only way this
  family gets a verdict.
- **src (after the wave)**: the twenty-second wave's two defects are still
  open and unchanged here (proc.syscall's comm filter after a rename;
  `sched.irq --filter 'kind == ...'` selecting nothing).

## Tests run

- `cargo test --test reference bpftrace::`: 19 passed.
- `cargo test --test reference pairs_bpftrace -- --nocapture`: 0 held, 10
  skipped, 0 gaps, 0 known defects, 0 failed.
- `every_pair_is_well_formed` and `every_row_names_a_pair_or_why_none`: red,
  on other families' pairs and rows only; neither names bpftrace.
- `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt
  --check`: clean.
