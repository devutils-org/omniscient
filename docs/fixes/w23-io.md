# The twenty-third wave, stream io

The io family's seven pairs, run by the shared runner. Branch
`feat/w23-io`: `ee1530d` (the pairs, parsers, captures and table) and this
handoff. The family had no harness of its own to delete: in the
twenty-second wave nothing ran its pairs.

## What landed

- `catalog/references/io.yaml`: seven pairs with ids, in the README's
  schema. `fio-reads` and `fio-clat` are `run: omn_launches_tool` (omn
  launches fio); `fio-bytes` and `fio-ios` are `run: alongside` with fio
  as the tool and `--startdelay=1`; `iostat-r-per-s`, `iostat-rkb-per-s`
  and `iostat-r-await` are `run: alongside` with fio's job as the workload
  started at `settle_ms: 1000`. Every pair has `setup: [fio,
  --create_only=1, ...]`, so no run lays the file out inside a window.
  Gone: `same_run`, `duration`, `start_after`, the `<emit>[<glob>].<field>`
  spelling, and the symmetric bands standing for one-sided ones.
- `tests/reference/io.rs`: `omn_summary_as_fio` and `omn_summary_as_iostat`,
  `omn`'s `summary` emit read in the tool's keys (requests summed over the
  series, the mean weighted by each series' count -- what the path grammar
  can sum but not weight). Nine unit tests, four new.
- `tests/reference/captures/io/`: all real, from this machine today.
  `fio-randread.json` (fio's report of the run omn launched),
  `omn-io-latency-launch.json` (that run's `results.json`),
  `iostat-dx.json` (`iostat -d -x -y -o JSON 4 1`, replacing the
  hand-written one) and `omn-io-latency-system.json` (the same four seconds:
  23375 reads, which is iostat's 5843.75 r/s times 4).
- `docs/replaces/io.md`: every row's verdict is the runner's, with its id.

## Each pair's outcome

Three full runs of `pairs_io` (the last on the committed YAML), each
`7 held, 0 skipped, 0 gaps, 0 known defects, 0 failed`, with the other
families measuring the machine at once (load average about 35).

| Pair | Outcome | Numbers (omn / tool) |
|---|---|---|
| `fio-reads` | holds, `rel 0.01` | 23798/23798, 20946/20946, 24745/24745 |
| `fio-clat` | holds, `rel 0.5, side: below`, medians of 3 | 57.3/83.1, 56.1/85.3, 56.0/76.3 us |
| `fio-bytes` | holds, `rel 0.05, side: above` | 102223872/102223872, 109830144/109805568, 112525312/112525312 |
| `fio-ios` | holds, `rel 0.05, side: above` | 20720/20709, 27744/27741, 24534/24530 |
| `iostat-r-per-s` | holds, `rel 0.02` | 5879.17/5879.25, 6348.90/6349.00, 7035.91/7036.00 |
| `iostat-rkb-per-s` | holds, `rel 0.02` | within 0.002 % each run |
| `iostat-r-await` | holds, `abs 10000` ns | 56.9, 56.0, 61.5 us against 0.06 ms |

## Shown vs reasoned

- **Shown**: the three runs above, through the runner, with
  `target/debug/omn` carrying `cap_bpf,cap_perfmon` (`make`). The parser
  tests on the real captures (`cargo test --test reference io::`, 9
  passed). `every_pair_is_well_formed` and
  `every_row_names_a_pair_or_why_none` name nothing of io (both stay red
  for other families: gpu, net, perfrecord, perfsched, perfstat, power,
  sysstat, top, bpftrace in the first; heap, sysstat and top rows in the
  second, one of which is sysstat's row pointing at `docs/replaces/io.md`).
  `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt
  --check`: clean.
- **Shown by hand before writing the pairs**: an iostat window inside the
  job (`iostat 1 3`, last report, against omn's 2 s) differed by -0.1 % to
  -14 % under the wave's load, because the job's rate moves within its run;
  windows holding the whole job agree within 0.01 %. That is why the iostat
  pairs watch four seconds around a two-second job.
- **Reasoned, not run**: the pairs on a quiet machine (the coordinator's
  run); `fio-clat`'s band there (18 % below in the twenty-second wave, well
  inside 50 %).

## Defects, by whose they are

- **The pairs (fixed here)**: the twenty-second wave's `iostat` pairs
  compared rates over two windows inside a moving job; `fio-clat`'s
  quarter band did not hold under load (36 % below once, at 88.5 us
  against 56.9), because the wake-up share of fio's clat waits for a cpu.
  The band is now half of fio's, one-sided, and its `why` says the share
  has no bound of its own.
- **omn**: none found by these pairs. The twenty-second wave's gaps stand
  (`docs/fixes/w22-io.md`, "Gaps in omn"): a launched program's forked
  children not followed by `io.latency`, no per-process `io.bytes` /
  `io.iops` collector, `--system` refused beside a program, no `%util`.
- **The runner**: under `omn_launches_tool` the "workload alone" check
  runs `workload` as a program, but the program the tool launches there
  is `command` + `workload`; a job file as the workload would fail that
  check with "the workload does not start" on the way to a legitimate skip
  (`omn` exit 1 on a machine without the capabilities). The io pairs put the
  job file in `command` with `workload: []`, which the check skips. Filed
  below.

## Not done, and why

- No pair for `iostat`'s merges, `aqu-sz`, `rareq-sz`, `d/s`, `f/s`,
  `%util`, or fio's `disk_util`: the workload merges, discards and flushes
  nothing and its requests are all 4 KiB; `%util` has no source. Each row
  says so with `no pair:`.
- Percentile pairs (fio p50/p99 against `io.latency -e hist`): not
  written; the parser already returns `read.clat_p50_ns` and
  `read.clat_p99_ns`, and an `omn_parse` reading the histogram with
  `log2_buckets` would be the pair.
- No `make check`, no `cargo test --workspace`, per the brief.

## Privileged runs wanted

None: every io pair needs only the file capabilities `make` grants, and
every witness (fio, iostat) runs without root.

## Decision lines

- **The job file goes in `command`, not `workload`, under
  `omn_launches_tool`**: the workload-alone check would run a job file as a
  program.
- **iostat's window holds the whole job** (`-y 4 1`, omn `-d 4s`, the job
  started at one second): a rate over a window inside a job moves with the
  job's own rate under load, a rate over the whole job is its reads over
  four seconds on both sides.
- **`setup` lays the file out before every run**, so no pair sees the
  layout's writes or a cold file's checksum reads.
- **A system-wide `io.latency` is held to reads by `--filter "rwbs =~
  'R'"`**, since it has no `dir` key; a launched process's sees only the
  reads it submits.
- **`omn`'s mean over devices is a parser**, weighted by count; the count is
  a path (`series.*.count`).
- **`iostat-r-await` is symmetric** although io.latency's span is inside
  r_await's: iostat rounds to 10 us, and was seen 1.5 us below omn's mean.

## Needs from another stream

- **runner**: under `omn_launches_tool`, run `command` + `workload` (the
  program the tool is) in `workload_fails_alone`, or document that the
  workload must be a program on its own.
- **sysstat**: its `docs/replaces/sysstat.md` row ``iostat -d`, `iostat
  -x`` points at `docs/replaces/io.md` and fails
  `every_row_names_a_pair_or_why_none`; the io pairs' ids it can cite are
  `iostat-r-per-s`, `iostat-rkb-per-s`, `iostat-r-await`. The iostat parser
  here is `iostat_json`, if sysstat still has one of its own.
- **coordinator**: `docs/replaces.md` -- io's seven pairs hold; `fio-clat`
  is "within 50 %, omn below", not "same numbers".
- **src (later)**: the twenty-second wave's four io gaps above; a `dir` key
  on `io.latency`, so a read/write split needs no filter.
