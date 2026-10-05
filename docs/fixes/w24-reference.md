# The twenty-fourth wave, stream reference

Branch `feat/w24-reference`, merged with `main` three times: at stream
run's merge, at power's and lazy's, then at cpu's, gpu's and emit's.
Nothing outside the row's files changed, and no shared layer. Every item of
`BRIEF.md`'s "reference" section landed. Item 4 is half done: stream cpu's
fix retired the leading-event hold, and `pidstat-w`'s marker waits on
stream sched (below).

## What landed

### 1. The runner (`tests/reference/runner.rs`, `9d7d61c`, `05ca273`)

- **The root check, by command.** This was already the runner's behaviour
  (`sudo_grants(tool)` asks `sudo -n -l` for a `NOPASSWD` entry naming the
  tool's path). The brief's "net's tcpdump pair skips today" was out of
  date. Now that sudoers grants `/usr/bin/tcpdump`, the pair runs and holds:
  `make CAPTURE=1`, then `OMN_REFERENCE_PAIR=tcpdump-captured`. The result
  was omn 1810 against tcpdump 1810.
- **`teardown: [argv]`.** It runs after each side under `separate`, and
  after the one run in the other modes. It runs whatever the side came to:
  a failure, a skip, or a `setup` that failed half-way. A failing teardown
  fails the pair, quoting it, after the side's own failure when there was
  one. Placeholders are substituted as in `setup`, and `{pid}` is refused.
  perfstat's `cpu.instructions.cgroup` now removes `omn-reference-perfstat`
  with it. Shown: the pair held (omn 4749211846 against perf 4749211475),
  and the cgroup was gone afterwards.
- **Under `omn_launches_tool`, `workload_fails_alone` runs the program omn
  launches**, which is `<command> <workload>` (through `sudo -n` when the
  pair is privileged). If that program fails alone with a refusal wording,
  the skip stands. If it fails alone for any other reason, the pair fails,
  saying "the program omn launches (the tool and the workload) fails with
  no observer". A job file as the workload no longer reads as "the workload
  does not start".
- **A path in `requires` that is absent** skips as ``` `/dev/nvidiactl`
  does not exist here ```. A binary still skips as "is not installed here".
  `.github/expected-skips.txt` has the new line, checked through
  `skip-gate.sh` on a sample line.
- **Exit 1 is decided by omn's last line**, now that stream run prints it
  (`234faa0`). `exit 1: the machine refused the run` skips, after the
  workload-alone check. `exit 1: the run failed after it started` fails,
  whether or not results were written. An `omn` that does not print the
  line falls back to the old heuristics.
- `catalog/references/README.md` documents all of the above, and the skip
  table has the new rows.

### 2. The pairs left

- **`top-cpus`: the bias was a defect in `omn`, found and named.** `omn`
  was below `top` by about a third in the twenty-third wave's quiet runs
  (0.140 against 0.204, 0.076 against 0.120). The cause is `cpu.util`'s
  first poll, which comes at once after its baseline read. That poll's
  share is a few ticks over a fraction of a millisecond: 0.0 on a quiet
  machine, 0.5 or 1.0 on a busy one. The summary's mean weighs it like a
  whole second, as one value of three. This was shown on this machine with
  `omn trace cpu.util --system -m poll -e raw -d 3s [--window 1s..3s]`:
  - reads at +0.2 ms (busy 0.0), +1.0 s (0.35), +2.0 s (0.316) and
    +3.0 s (0.281);
  - with the window, a read at +0.3 ms past the window's opening (1.0),
    then 0.846 and 0.814;
  - when that read is "unmoved" it is dropped, and the summary has one
    value fewer.

  The pair now reads the raw polls through the new parser
  `omn_cpu_util_polls`, which then left out a read less than one tick (10 ms)
  after the window opened. It is unit-tested on a real capture
  (`captures/top/omn-cpu-util-raw-window.json`). Three runs at a load of 12
  (not quiet) gave omn 0.525 / 0.501 / 0.607 against top 0.536 / 0.516 /
  0.595: omn below twice and above once, 0.011 to 0.015 apart. **Stream
  cpu fixed the poll in the same wave** (`c13eb51`: the first sample is
  the baseline). After merging it, the parser left out nothing: 0.195
  against 0.203, and 0.632 against 0.624. In the fix round it stopped
  leaving the read out and refuses it instead (see "Review"): leaving it
  out would have masked the defect's return, the pair passing on the reads
  around it.
- **`load-latency`** held under load: omn 366.1 against perf mem 368.1
  cycles, medians of five. The twenty-third wave's quiet run held too, so
  no `known_defect` was added.
- **gpu's four `omn_*.json` captures** were recaptured from real runs with
  the driver loaded, using the commands of `w23-gpu.md`'s "Privileged runs
  wanted", step 2. The parser tests' numbers moved to them:
  - copy 312925 ns;
  - `[memcpy HtoD]` 1815081 ns;
  - copy `dram__bytes_read.sum` 16840992.

  `docs/replaces/gpu.md` no longer calls them composed. After stream
  gpu's merge, with nothing else on the device, `pairs_gpu` gave **21
  held**.
- **bpftrace's two medians.** `biolatency` held at log2 buckets 16 against
  15. `runqlat` was a gap because omn could not watch the machine for a
  program's life. It now runs with `--system` beside the program (stream
  run's item 1) and `runs: 3`. It held twice (2097152 against 2048000,
  buckets 21 and 20) and went 2 buckets apart once at a load of 38. Single
  runs under the wave's load read 16384 against 1024000 once.
- **perfsched's perf pairs**: all nine held through the runner with `perf`
  granted. They were not moved to `alongside`. Their counts are exact on
  separate runs of a deterministic workload. The one pair that needed a
  single run, `perf-lock-contention`, is already `tool_wraps_omn` at
  `rel 0.02`. Moving `perf sched record` to `-a` would only add the
  machine's switches.
- **io's percentile pair `fio-clat-p50`**: omn's `io.latency -e hist`
  median bucket against fio's clat p50, at `log2_buckets: 1`, using the new
  parser `omn_hist_as_fio` (unit-tested on a real capture). It held: omn
  65536 against fio 150528 (buckets 16 and 17) under load. The capture's
  run read 32768 against 78336.
- **io's write pair `iostat-w-per-s`**: `w/s` against `io.iops`
  `[key=dir:write]`, over a new job `workloads/io/randwrite.fio` (random
  direct 4 KiB writes, 2 s, `end_fsync=1`). Six of six held within 1.8 %
  at a load of 12 to 23, five of them within 0.3 %. Without `end_fsync`,
  two runs were 3.4 % below and 7.7 % above.

### 3. Captures written from documentation, replaced by real output

- **bpftrace**: eleven `<tool>.txt` files now sit beside the
  `.format.txt`, each captured through `sudo -n` the way its pair runs it.
  `every_parser_reads_its_tools_real_output` asserts their numbers:
  - syscount 21215;
  - opensnoop 624;
  - killsnoop 50;
  - bitesize 508 / 502 in 4 KiB;
  - biosnoop 509;
  - biolatency p50 8000 us·1000;
  - runqlat 46441;
  - runqlen 2812;
  - tcpconnect 10;
  - execsnoop 20;
  - tcplife 20 and 640 kB.

  The system-wide captures (opensnoop, execsnoop) are trimmed to the
  workload's lines plus five others, leaving out other programs' paths. The
  repository path is spelt `/repo`. `vfsstat` and `swapin` (no pair)
  printed empty maps and were not kept. The format files stay for the
  cases a real run did not produce.
- **perfsched**: `perf-trace-s.txt` and `perf-sched-latency.txt` are real
  now, and the tests' numbers moved:
  - getppid 4998, because perf's own table lost two;
  - `omnref-sleep` 102 switches;
  - total 949.
- **heap**: `memleak.txt` is the pair's own command through `sudo -n`,
  with a new test asserting 7726413 bytes in 564 allocations. The pair
  stays a `gap` (memleak counts mmap).
- **power**: `cpupower_mperf.txt` is a real `sudo -n cpupower monitor -m
  Mperf` beside `spin.py 3 24`. The brief's "still uid 0" did not hold:
  sudoers grants cpupower and `msr` is loaded. The capture covers 24 cpus,
  with cpu1 at 4006 MHz. `rapl_window.txt` is real (293.04 J). Here
  `energy_uj` is readable by everyone, so that pair is unprivileged. The
  wrapped capture is still composed, and its test says so. All seven
  power pairs held, before and after stream power's merge. The two
  frequency pairs' `why` now names the `pmu` backend they resolve to since
  that merge.

### Defects found in the reference tools, refused rather than compared short

- **bpftrace `Lost N events`.** Under the wave's load, `opensnoop` once
  read 427 against omn's 623; alone it reads 624 against 623. The six
  event-line parsers now refuse an output carrying `Lost N events`. That
  the 427 was such a loss is reasoned, not shown: that run's stderr was not
  kept.
- **perf trace `LOST N events!`.** Under load this was shown three times
  by hand: 628, 2010 and 3318 lost, with getppid at 2334 of 5000, and once
  with no getppid row at all. `perf_trace_summary` now refuses such output.
- **execsnoop's omn side counted every `true`.** The filter was
  `comm == "true"`, so every program running `true` through PATH counted:
  171 against 93 while other streams ran `omn ... -- true`. It now filters
  on `filename == "/bin/true"`, the argv0 execsnoop is keyed by. That gave
  20 against 20 once, and 20 against 19 once, a tool-side miss with no
  "Lost" line.

## Shown vs reasoned

- **Shown**:
  - `cargo test --test reference -- runner::`: 22 passed, three of them
    new (`teardown_runs_after_each_side_whatever_it_came_to`,
    `the_program_omn_launches_is_what_runs_alone`,
    `omns_last_line_says_which_exit_1_it_is`), plus the path skip inside
    `the_machine_skips_and_the_data_fails`.
  - The parser modules' tests: `top::` 11, `gpu::` 16, `bpftrace::` 21,
    `perfsched::` 13, `heap::` 8, `power::` 9 and `io::` 11, all passing.
  - The data checks: `every_pair_is_well_formed` and
    `every_row_names_a_pair_or_why_none` pass.
- **Shown, the pairs, family by family, at loads of 12 to 38** (sixteen
  streams at once, never quiet):

  | Family | Held | Gaps | Known defects | Failed |
  |---|---|---|---|---|
  | power | 7 | | | |
  | top | 11 | | | |
  | sysstat | 12 | | 1 (`pidstat-w`) | |
  | heap | 8 | 1 | | |
  | net | 8 | | | |
  | perfrecord | 5 | | | |
  | io | 9 | | | |
  | perfsched | 9 | | | |
  | bpftrace | 10 | | | |
  | perfstat | 11 | | | 2 |
  | gpu | 21 | | | |

  - net is 8 held now that tcpdump runs; it was 7 held and 1 skipped.
  - io is 9 with the two new pairs.
  - bpftrace is 10 with runqlat. Its pairs were held one at a time.
  - **No pair skipped**: `sudo -n` grants every witness now.
- **Red only under load, each rerun alone once, filed and not widened**:
  - perfstat's `cpu.ipc`: omn 2.29 against perf 2.94 alone. By hand,
    `perf stat` alone read 2.62, 2.20 and 2.93 on three runs, so its
    cycles move 30 % between separate runs at a load of 20.
  - perfstat's `cpu.cache_misses`: held alone.
  - io's `fio-bytes`: omn above by other readers, 99 MB against 69 MB; the
    band is 5 %, above only.
  - bpftrace's `runqlat`: 2 buckets apart once at a load of 38.
  - sysstat's `vmstat-si` and `vmstat-so`: failed once during a burst of
    the machine's swapping (528 MB swapped out in the 4 s); they held
    alone.
  - perfsched's `perf-trace-count`: perf trace lost events, now refused by
    its parser.

  All of these are for the coordinator's quiet run.
- **Reasoned**:
  - that opensnoop's 427 was bpftrace's buffer;
  - that `top-cpus` holds on a quiet machine with the parser. The machine
    was never quiet: the parser removes exactly the read the raw events
    show, and the remaining polls cover top's seconds;
  - `pairs_gpu` with the recaptured files (not run: the device is stream
    gpu's this wave).
- `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt
  --check` are clean, last run after the first merge of `main`. No
  `make check`, no `cargo test --workspace`.

## Not done, and why

1. **Item 4: `pidstat-w`'s `known_defect`** (sysstat, `sched.switches`'
   counted-window rate) still reproduces on this branch. Stream sched has
   not merged yet. Once it does, the pair fails with "the defect no longer
   reproduces: remove `known_defect`", and the marker in
   `catalog/references/sysstat.yaml` is a one-line removal for whoever
   merges sched, or for the coordinator. **Done for the other marker**
   (`d79d4c8`). Stream cpu's leading-event fix made sysstat's
   `poll_leading_event_is_held_as_a_known_defect` fail, as designed. It is
   now `poll_has_no_leading_event` (stream cpu's name, kept at the
   fix round's merge, which took main's `sysstat.rs`), a guard over all four poll
   sources. `raw_means` no longer skips a first event. The two `omn`
   captures (cpu.util, cpu.pressure) are recaptured, and `omn_psi` drops
   the old `some` naming (stream cpu's request).
2. **New pairs stream power suggested** (`w24-power.md`): `Busy%`/`Avg_MHz`
   against turbostat, `energy-core` against `perf stat -a`, and `pidstat
   -w` cswch/s against `power.wakeups`. Not written. The first needs an
   `omn_parse` over raw events (the summary is over `hz` only).
3. **Pairs rewritten as `--system -- workload`** in place of a tool
   wrapping omn (perfstat 3, power 1, io 3, top 6, net), which stream run's
   item 1 allows now. Only bpftrace's runqlat was rewritten. The others
   hold as they are.
4. **The runner reading `lifetime.exit_code`** instead of the pidfd, under
   `omn_launches_tool`. Not done: the pidfd covers the tool, which is not
   omn's launched program when it runs through `sudo`.

## Privileged runs wanted

None beyond what `sudo -n` grants. For the coordinator's quiet run, alone
on the machine and with nothing on the GPU:

```console
make CAPTURE=1      # cap_net_raw for tcpdump-captured's net.capture
cargo test --test reference -- --test-threads=1 --nocapture
```

## Decision lines

- A tool's own reported loss (`Lost N events` from bpftrace, `LOST N
  events!` from perf trace) fails the parse naming it. It is never compared
  as a count: a short count is neither omn's defect nor the machine's
  refusal.
- `teardown:` mirrors `setup:`. It always runs, and a failure there fails
  the pair.
- Under `omn_launches_tool`, what runs alone before a skip is the program
  omn launches (the tool with the workload), not the workload.
- A missing path in `requires` "does not exist here", while a missing
  binary "is not installed here".
- Exit 1 is decided by omn's last line first, and by the heuristics only
  when the line is absent.

## Needs from another stream

- **cpu**: none left. The `cpu.util` first-poll defect this stream found
  through `top-cpus` was fixed by stream cpu in the same wave (`c13eb51`),
  and the merged branch shows it: no leading event.
- **sched**: when the counted-window fix lands, remove `known_defect` from
  `pidstat-w` in `catalog/references/sysstat.yaml` (item 4 above).
- **coordinator** (`docs/open-findings.md`):
  - the `cpu.util` first-poll defect, which is fixed: the explanation of
    the twenty-third wave's `top-cpus` filing;
  - the tools' own losses under load (bpftrace, perf trace);
  - `cpu.ipc`'s separate-run spread under load.

  For `STATE.md`: no pair skips on this machine any more.

## Review

- **`omn_cpu_util_polls` hid the read it claimed to guard** (`top.rs`,
  gap). The reviewer is right: it dropped a `cpu.util` event stamped less
  than a tick after the window opened, counted nothing and reported
  nothing, so the defect's return would have left `top-cpus` green. Fixed:
  the parser now returns an error naming defect 2 of
  `docs/fixes/w22-sysstat.md` when it finds such an event, and the pair
  fails with it. Its unit test holds both sides on the real pre-fix
  capture: the capture as taken is refused, the same capture without its
  leading read gives (0.846 + 0.814) / 2. `top.yaml`'s `why`,
  `docs/replaces/top.md` and this handoff say "refuses" in place of
  "leaves out" and "stays as a guard". sysstat's `poll_has_no_leading_event`
  remains the guard over all four poll sources; this is the pair's own.
- **Merge of `main`**: `tests/reference/sysstat.rs` and its two `omn`
  captures conflicted with stream cpu's `8e4d08b`, which made the same
  change (hold to guard, `raw_means` keeping every event, captures
  retaken). Main's side was taken whole; the handoff now names its test.
