# The twenty-second wave, stream perfrecord

`perf record`/`report`, `perf top`, `perf mem` and `perf c2c` against
`cpu.profile`, `mem.latency` and `mem.sharing`. The verdict table is
`docs/replaces/perfrecord.md`.

## What landed

- `catalog/references/perfrecord.yaml`: seven pairs.
  - `cpu.profile` / `perf record` + `perf report --no-children --sort dso,sym`,
    keyed `hot_beta` and `hot_alpha`: a C workload (`hot.c`) whose three leaf
    functions do 20, 50 and 30 % of every round by construction.
  - `cpu.profile` / `perf record` on `python3 split.py`, keyed
    `libcrypto.so.3`: 60 % of the wall time hashes in a stripped library, so
    both sides fold its unnamed addresses into the object.
  - `cpu.profile` / `perf top -p`, keyed `hot_beta`: `perf top --stdio`
    attached to the launched pid, stopped by SIGINT, its last screen parsed.
  - `mem.latency` / `perf mem record` (mean weight of the timed loads, read
    with `perf script -F weight`): a pointer chase through 64 MiB (`chase.c`).
  - `mem.sharing` / `perf c2c record` + `report`, keyed `top`: two threads
    writing two counters in one static, non-PIE line (`share.c`).
- One field beyond the shared schema: **`compare.omn_parse`**, the name of a
  parser that reads omn's JSON. None of these values sits at one JSON path: a
  function's self share is its stacks' leaves summed over the stack table, a
  mean latency is a histogram's sum over its count, a line's share is a top-k
  row over the total. `compare.omn` describes the place in words.
- `tests/reference/perfrecord.rs`: seven parsers -- `perf_report_self`,
  `perf_top_self`, `perf_script_weight_mean`, `perf_c2c_hitm_share` for the
  tools, `omn_profile_self`, `omn_hist_mean`, `omn_topk_share` for omn --
  each unit-tested on a capture in `tests/reference/captures/perfrecord/`
  (eight tests). And an ignored test, `pairs_hold_on_this_machine`, that runs
  every pair of the YAML with these parsers on both sides, until the shared
  runner reads `compare.omn_parse`.
- `tests/reference/workloads/perfrecord/`: `hot.c`, `chase.c`, `share.c`,
  `split.py`, and `build.sh NAME ARGS`, which builds `NAME.c` once into
  `target/reference/perfrecord-NAME` (or `$CARGO_TARGET_TMPDIR/reference/`)
  and `exec`s it, so the profiled process is the program alone.
- `docs/replaces/perfrecord.md`: eight rows.

## Shown versus reasoned

Shown, `cargo test --test reference perfrecord -- --ignored --nocapture`,
twice in a row, on the AMD Ryzen 9 3900X, kernel 7.2, `perf` 7.2.8,
`perf_event_paranoid` -1, load average about 20 from the other streams:

| Pair | Tool | omn | Tolerance |
|---|---|---|---|
| `cpu.profile` / `perf record`, `hot_beta` | 0.501, 0.496 | 0.503, 0.500 | 0.05 abs |
| `cpu.profile` / `perf record`, `hot_alpha` | 0.193, 0.198 | 0.201, 0.203 | 0.05 abs |
| `cpu.profile` / `perf record`, `libcrypto.so.3` | 0.596, 0.596 | 0.597, 0.595 | 0.05 abs |
| `cpu.profile` / `perf top`, `hot_beta` | 0.500, 0.499 | 0.499, 0.497 | 0.05 abs |
| `mem.latency` / `perf mem`, mean cycles | 276.6, 262.3 | 268.5, 272.4 | 10 % |
| `mem.sharing` / `perf c2c`, top line's HITM share | 1.000, 1.000 | 1.000, 1.000 | 0.02 abs |

Also shown, by hand: both name the false-shared line `0x4040c0` (the
captures `perf-c2c-share.txt` and `omn-sharing-share.json` hold it); `omn
diff A A` prints the same per-function "own" table the parser sums.

Reasoned, not shown: that `--children` inclusive shares would match (they are
sums over the same stacks); that `omn top --view flame --pid` shows the
numbers the `perf top` pair compares (it draws `cpu.profile`'s stacks; no
pair can drive an interactive view); the system-wide and cgroup targets of
`perf record -a` / `-G` (the same collector, other targets, no pair).

Tolerances are wide because the runs alternate (each side launches the
workload) and the machine is loaded: perf samples cycles in frequency mode,
so its overhead is period-weighted and moves by about 2 points between runs
(seen: `hot_gamma` 0.28 to 0.35 over five runs); omn samples cpu-clock.

## The suite

`cargo test --test reference`: 8 passed, 1 ignored (the end-to-end test
above, run by hand). `cargo clippy --workspace --all-targets -- -D warnings`
and `cargo fmt --check` clean.

## Not done, and why

- **No pair runs through the shared runner yet**: the runner was not on
  `main` when this stream worked, and these pairs need `compare.omn_parse`
  (below). Until then the ignored test is what shows the numbers.
- **Same execution**: every pair is on alternate runs. `perf record -p` /
  `perf top -p` could attach to a run omn launched, but the schema runs the
  tool with the workload appended and omn with `--`, and has no way to hand
  one side the other's pid.
- **`perf annotate`**, **`perf record -e <any event>`**: omn has no
  equivalent to pair (gaps below).
- **`perf mem report --sort mem`** (shares by level): `level` is a filter
  field of `mem.latency`, not a key, so the per-level breakdown is one run per
  level; not paired.

## Gaps found (each a line of work for `omn`)

1. **`cpu.profile` on `pmu:sw` has no `comm` field to filter on.** `omn
   trace cpu.profile -m sample@997hz -e stack --filter 'comm ==
   "perfrecord-hot"' -- target/reference/perfrecord-hot 1` keeps no sample,
   but says so: the text output prints `filter `comm == 'perfrecord-hot'`
   kept 0 of 997 events, evaluated after reading`, and the JSON carries
   `"filtered": {"kept": 0, "dropped": 998, "unseen_fields": ["comm"]}`. The
   loss is counted and surfaced, and `unseen_fields` names the cause: the
   `pmu:sw` backend never fills `comm`. Not a defect against an invariant
   (an earlier draft of this handoff called it a silent loss; the review
   corrected it). The line of work: fill `comm` on the sampled events, or
   refuse a filter on a field the resolved backend never fills before the
   run rather than after it.
2. **`cpu.profile` has no cycles collector**: `pmu` (cycles) says "no
   collector yet", `pmu:sw` (cpu-clock) serves. `perf record`'s default is
   cycles.
3. **No flat per-function table in `trace`'s output**: the top functions by
   self (and by inclusive) share are what `perf report` prints first; omn
   prints stacks by id and a stack table, and only `omn diff` prints a
   per-function table. Wanted: `-e topk` keyed by function (leaf and
   inclusive), or a `func` key.
4. **No per-object key** (`perf report --sort dso`).
5. **`mem.latency` by level as a key**, not only a filter field (`perf mem
   report --sort mem`).
6. **`mem.sharing`'s per-line detail**: offsets within the line, the code
   addresses and pids touching it, loads against stores (`perf c2c report`'s
   Pareto table).
7. **Instruction-level attribution** (`perf annotate`, `perf top`'s `s`).
8. **Sampling on an arbitrary PMU event or tracepoint** (`perf record -e`).
9. Small: `omn diff A A`'s tables list functions alphabetically when nothing
   moved, not by share.

## Privileged runs wanted

None. Every pair runs unprivileged here (`perf_event_paranoid` -1; IBS for
`perf mem`/`perf c2c` and `mem.*` is open at that setting). On a machine at
the kernel's default (2), `perf mem`, `perf c2c` and the kernel frames of
`perf record` need `CAP_PERFMON` for `perf` too:

```console
sudo setcap cap_perfmon,cap_sys_ptrace+ep "$(command -v perf)"
cd /home/moha/repos/omniscient && make && cargo test --test reference perfrecord -- --ignored --nocapture
```

## Decision lines

- A family's pair names omn's parser (`compare.omn_parse`) when omn's value
  is computed from its JSON rather than read at one path; the family module
  parses both sides.
- A function is keyed by its symbol, or by its object when the tool printed
  an address, on both sides; a stripped library is one bucket.
- C workloads are built once by `build.sh` into `target/reference/` and
  `exec`ed, built without PIE so a static line has a fixed address.

## Needs from another stream

### To reference (`tests/reference.rs`, `catalog/references/README.md`)

- Read **`compare.omn_parse`**: when present, call that parser (from
  `all_parsers()`) on omn's stdout and stderr and compare its value under
  `compare.key`, instead of reading `compare.omn` as a JSON path. Document it
  in the README. Then `pairs_hold_on_this_machine` can be deleted.
- Run omn as `target/debug/omn <omn...> -- <workload...>` with the
  repository root as the working directory: the workloads' paths are
  relative to it.
- The `perfrecord` pairs take some 40 s together (seven pairs, two runs of 2 s
  plus perf's post-processing each).

### To the coordinator (`STATE.md` "What remains")

- Gaps 1 to 9 above, all features. Gap 1 is not a defect: the drop is
  counted and its cause named (see "Review"); file it as a missing field.

## Review

All three findings were right; fixed in the fix round.

1. **Gap 1 was wrong.** Rerun on a fresh build: the text output prints
   `filter ... kept 0 of 997 events, evaluated after reading` and the JSON's
   `filtered` holds `kept: 0, dropped: 998, unseen_fields: ["comm"]`. The
   loss is counted and surfaced; the gap is a field `pmu:sw` never fills.
   Gap 1 and `docs/replaces/perfrecord.md` are reworded; the coordinator
   line no longer calls it a defect. Other sources refuse a `comm` filter on
   a backend without the field before the run (`docs/scenarios/io.md`);
   doing that here is the line of work.
2. **`pairs_hold_on_this_machine` read a missing key as 0.** A side without
   the compared key now fails the pair, naming which side lacks it and the
   keys each side has.
3. **The `mem.sharing` / `perf c2c` pair did not show the same line.** A
   second pair compares key `0x4040c0` (the line's start, fixed by
   `-no-pie`); with finding 2 fixed, a side that names another line fails
   it. The `top` pair stays, and its `why` says it checks the share only.

Shown in the fix round, `cargo test --test reference perfrecord -- --ignored
--nocapture`, twice, load average about 9 with the other streams' fix
rounds: the new pair keyed `0x4040c0` passed both times (1.000 vs 1.000), the
`top` pair both times (1.000 / 0.993 vs 1.000). Each run failed one untouched
pair, a different one each time: `mem.latency` / `perf mem` (370.7 vs 289.5
cycles, then 268.0 vs 277.6, inside) and `cpu.profile` / `perf record`
`hot_beta` (0.340 vs 0.460, after 0.500 vs 0.501). That is the run-to-run
spread of perf's cycles-mode sampling and of miss latency on a loaded
machine named under "Shown versus reasoned", not something this round
changed; whether these tolerances are wide enough under load is open.
