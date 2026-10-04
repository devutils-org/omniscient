# The twenty-third wave, stream runner

The shared runner learns what the families asked of it in the twenty-second
wave, as code, each with a validation rule and a unit test on a stub tool.
Branch `feat/w23-runner`: `16c860a` (the runner), `3ac857e` (the schema,
the skip lines), and this handoff.

**Families, read this first** (the README's diff, summarised), then
`catalog/references/README.md` whole: it is the schema, and the runner now
refuses anything it does not describe.

## The schema's diff, for the families

| New | What it is | Asked by |
|---|---|---|
| a field the runner does not read is **refused** | `every_pair_is_well_formed` names it (`` `same_run` is not a field the runner reads ``), and the pair fails before it runs. Prose goes in `why`, `use`, `gap` or a YAML comment | the brief |
| `compare.omn_parse: <name>` | a `Parser` of the family applied to `omn`'s output (`{out}/results.json` with `-o {out}`, its stdout otherwise, as `stdout`; its stderr as `stderr`), compared key by key like the tool's; exclusive with `compare.omn`; `omn`'s output need not be JSON then | perfrecord, and every grammar the families invented |
| `run: alongside` | **a name for `attach` with no `{pid}`**, not a mechanism of its own: same start-up, settle and wait, `{pid}` refused; the workload is optional (none: both observers run side by side on their own duration) | io, net, bpftrace, sysstat (`target: system`) |
| `run: tool_wraps_omn` | `<command> <omn binary> <omn> -- <workload>`, one process tree; `-o {out}` is required in `omn`; the tool's parser reads the whole command's stdout and stderr | power, perfstat |
| `run: omn_launches_tool` | `omn <omn> -- sh -c 'exec "$@" >tool.stdout 2>tool.stderr' ... <command> <workload>`: omn watches the tool's own pid, the tool's output goes to two files its parser reads, never into omn's | io, gpu |
| `setup: [argv]` | run before each side (`separate`) or before the one run; must exit 0; placeholders substituted | perfstat |
| `reference.requires_setting: [{file, at_most / at_least}]` | an integer file and its bound; past it, a skip naming the file, its value and the bound; unreadable, a skip; not an integer, a failure | power |
| `tolerance: {rel/abs, side: above / below}` | one-sided: `above` holds for `0 <= omn - tool <= band`, `below` for `-band <= omn - tool <= 0` | io |
| `tolerance: {log2_buckets: N}` | `floor(log2)` of both at most N apart; 0 or below is in no bucket and equals only another such; stands alone | bpftrace |
| `known_defect: "<finding>"` | runs; outside tolerance prints `known defect:` and passes as a known defect; inside, fails "the defect no longer reproduces: remove `known_defect`"; a path or key error still fails; not with `gap` | sysstat |
| the tally | `pairs_<family>` ends on `tally: reference <family>: H held, S skipped, G gaps, K known defects, F failed`; `reference_tally` (ignored by default) reads every family's after the fact | the brief |

Unchanged: `separate`, `attach`, the placeholders, `{key}`, `report`,
`requires`, `privileged`, `refused_when`, the skip rules, `omn`'s exit
codes, the table rules. The path grammar is unchanged on purpose: `.0` not
`[0]`, `.*` not `[*]`, one `[field=value]` filter per segment, no value
containing `]`, no arithmetic. Anything else is an `omn_parse` parser.

### What each family's current fields become

| Family | Field today | Becomes |
|---|---|---|
| bpftrace | `alongside: true` | `run: alongside` |
| bpftrace | `hist.p50`, `hist.count`, medians held "to one bucket" | `omn_parse` parsers; `tolerance: {log2_buckets: 1}` |
| gpu | `same_run: omn_launches_tool` / `alternate` | `run: omn_launches_tool` / `run: separate` with `runs: N` |
| gpu | `compare.omn_jq` | gone: `compare.omn` where the path grammar reaches, `omn_parse` otherwise |
| gpu | `rate[*].per_second`, `raw[kernel=copy,counter=...]`, `topk[kernel=\[memcpy HtoD\]]` | `rate.*.per_second`; the two-attribute and bracketed selectors are `omn_parse` |
| heap | `topk.total`, `summary.max` (`<emit>.<field>`) | a path (`results.0.outputs[emit=topk].series.0.total`, say) or `omn_parse` |
| io | `same_run: omn_launches_tool` / `alongside`, `duration`, `start_after` | `run: omn_launches_tool` / `run: alongside`; omn's own `-d`; `settle_ms` |
| io | `rate[dir:read].total`, `summary[*].mean` | `[key=dir:read]` filters, `.*`, or `omn_parse` (a mean weighted by count is a parser) |
| io | symmetric bands standing for one-sided ones | `side: above` / `side: below` |
| net | `alongside: {lead_ms}` | `run: alongside` with `settle_ms`; the tool started `lead_ms` after omn is **not** offered (below) |
| perfrecord | `compare.omn` holding prose beside `omn_parse` | drop `compare.omn`: the two are exclusive |
| perfsched | `compare.across: sum` | the path's own sum over `*` and filters |
| perfsched, sysstat | `compare.scale` | `omn_scale` / `reference_scale` |
| perfstat | `same_run: tool_wraps_omn` | `run: tool_wraps_omn` -- but read the caveat below first |
| perfstat | `setup: spin_pid` | `run: attach` with `spin.py` as the workload and `{pid}` |
| perfstat | `setup: cgroup_spin` | not offered (a background process in a cgroup the runner makes and removes): a `gap:` |
| perfstat, top | `compare.omn_jq` | gone, as gpu |
| power | `same_run: tool_wraps_omn` / `back_to_back` | `run: tool_wraps_omn` / `run: separate` |
| power | `reference.requires_setting: [{file, at_most}]` | as written: the runner reads exactly that |
| power | `background: true`, `compare.omn_series` | not offered; `[key=...]` filters for the series |
| sysstat | `target: system` | `run: alongside` |
| sysstat | `compare.omn_key`, `raw.<attr>` means | `[key=...]` filters; `omn_parse` for a mean over raw events |
| sysstat | `known_defect` | as written |
| top | `same_run: onpid_attach` | `run: attach`: the runner substitutes `{pid}` itself now; `onpid.py` can go |

## Shown vs reasoned

- **Shown** (`cargo test --test reference -- runner:: common::
  a_row_is_backed families_are_whole`, 21 passed): every new behaviour has a
  unit test on `sh` standing in for `omn` and the tool --
  `omn_parse_reads_omns_output_with_a_parser`,
  `a_field_the_runner_does_not_read_is_refused` (including an old
  `setup: cgroup_spin` string, which loads and is refused rather than
  failing the whole file), `the_tool_wraps_omn_in_one_run`,
  `omn_launches_the_tool_in_one_run` (the tool's stdout kept out of omn's,
  and a refusal on its stderr skipping), `alongside_starts_both_then_the_workload`
  (with and without a workload, `{pid}` refused), `setup_runs_before_each_side`
  (omn sees one setup, the tool two; a failing setup fails quoting it),
  `a_setting_past_its_bound_skips` (at most, at least, unreadable,
  not an integer, no bound), `one_sided_tolerances_allow_one_side_only`,
  `log2_buckets_count_one_bucket_apart_as_equal`,
  `a_known_defect_holds_until_the_numbers_agree`,
  `the_tally_reads_the_same_in_every_family`. The eight earlier runner tests
  still pass.
- **Shown, the whole reference binary but the pairs**
  (`cargo test --test reference -- --skip pairs_`): 123 passed, 2 failed --
  `every_pair_is_well_formed` and `every_row_names_a_pair_or_why_none`, both
  red on `main` before this stream; the second names the same 22 rows as on
  `main`.
- **Shown, a real family through the runner**: `make`, then `pairs_heap`
  ran valgrind (dhat, massif) and heaptrack against `omn` and parsed them;
  1 skipped (`memleak`, `sudo -n`), 8 failed on the family's `<emit>.<field>`
  paths (`nothing at Field("topk")`), the family's rewrite. Its tally line
  printed and was left for `reference_tally`, which then read it, printed
  "not run" for the ten others, and failed naming heap.
- **Shown by hand with the real `omn` and `perf`**: the `omn_launches_tool`
  shape (`omn cpu.instructions ... -- sh -c 'o=$1; e=$2; shift 2; exec "$@"
  >"$o" 2>"$e"' sh ... perf stat -x, -e instructions -- python3 ...`) leaves
  omn's stdout clean and `perf stat`'s CSV in the files; and the caveat
  below.
- `.github/expected-skips.txt`: the two new skip lines checked through
  `.github/scripts/skip-gate.sh` on sample lines.
- `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo fmt --check`: clean.
- **Reasoned, not run**: no family pair has yet run in the three new modes
  with a real witness through the runner (every one is rejected as data
  until its family rewrites it); `privileged: true` under
  `omn_launches_tool` (`sudo -n` exec'd by the shell) has run only in the
  documented shape, not with a granted sudo.

### A finding: a perf-based tool wrapping the capable `omn` counts nothing

`perf stat -x, -e instructions:u -- target/debug/omn trace cpu.instructions
-m count -e summary -o X -- python3 -c '...'` reads **0** instructions (4750
ns enabled) while `omn` carries `cap_perfmon,cap_bpf=ep`, and **963 480
494** over a copy of the same binary without capabilities. The kernel ends a
task's inherited perf events when it execs a binary that is not dumpable,
which a binary with file capabilities is. So a `tool_wraps_omn` pair whose
tool counts through inherited perf events (`perf stat --`) reads 0 rather
than skipping. The README says so under `tool_wraps_omn`: such a pair is
`omn_launches_tool` or `separate`; wrapping `turbostat` or RAPL energy is
unaffected. For `docs/open-findings.md` (coordinator).

## Not done, and why

- **net's `lead_ms`** (the tool started some time after `omn`): both
  observers start together under `alongside`. Not added: one more timing
  field for one family, and `settle_ms` plus `omn`'s own `-d` cover the
  window. net declares `gap:` with the reason if it truly needs it.
- **power's `background: true`** and **perfstat's `setup: cgroup_spin`**:
  a process started before the run and killed after, or one placed in a
  cgroup the runner makes and removes. `setup` is an argv that must finish;
  a process that must outlive it is a workload. Not added; those pairs are
  `attach` with the spinner as the workload, or `gap:`.
- **Two-attribute filters, values containing `]`, `[*]`, arithmetic in a
  path**: not added, on purpose -- the brief settles them as `omn_parse`.
- **The `omn` exit 1 that is "failed mid-run" rather than "this machine"**
  (carried from the twenty-second wave) still needs `omn` to say which;
  `src/`'s.
- No `make check`, no `cargo test --workspace`, per the brief.

## Privileged runs wanted

None for the runner itself. To see `privileged: true` and
`omn_launches_tool` together once a family has such a pair:

```console
sudo -v   # by a person, once
make && cargo test --test reference pairs_<family> -- --nocapture
```

## Decision lines

- **Unknown fields are refused per pair, not per file**: they are collected
  (`#[serde(flatten)]`) and named by `Pair::problems`, so a family file with
  one stale field still loads, its other pairs still run and its table
  rows still resolve. `setup` written as a string (`cgroup_spin`) likewise
  loads and is refused by name.
- **`alongside` is a name for `attach` without `{pid}`**, sharing its
  start-up; the only differences are `{pid}` refused and the workload
  optional.
- **`tool_wraps_omn` requires `-o {out}`**: the two outputs share a stdout,
  and a path into a mixed stream would be luck.
- **`omn_launches_tool` `exec`s the tool**, so omn watches the tool's own
  pid (a per-pid key agrees); the price is that the tool's exit status is
  not seen, so its refusal is read from its stderr when its output does not
  parse.
- A one-sided tolerance allows nothing on the other side, not even noise:
  the pair's `why` says why `omn` can only be on one side.
- `log2_buckets` stands alone and treats a value not above 0 as no bucket,
  so a lost median (0) never holds against a real one.
- A known defect excuses only numbers outside the tolerance; a path that
  finds nothing or a key a side lacks still fails.
- The tally of a filtered run (`OMN_REFERENCE_PAIR`) is printed but not
  left for `reference_tally`: it is not the family's.

## Needs from another stream

- **Every family (phase 2)**: rewrite to the table above; on this branch
  `every_pair_is_well_formed` refuses pairs in ten families (all but heap,
  whose eight paths fail at run time instead), in pairs of the file:
  bpftrace 2 of 10, gpu 21 of 21, io 7 of 7, net 9 of 9, perfrecord 7 of 7
  (`omn` beside `omn_parse`), perfsched 7 of 9, perfstat 13 of 13, power 7
  of 7, sysstat 11 of 13, top 11 of 11; the test names each field.
- **perfstat, power**: read the `tool_wraps_omn` caveat before choosing it
  for `perf stat`.
- **Coordinator**: file the capability/perf-inherit finding above in
  `docs/open-findings.md`; after every family merges, run
  `cargo test --test reference pairs_ -- --nocapture` then
  `cargo test --test reference reference_tally -- --ignored --nocapture`
  for the wave's table.
- **src (later)**: `omn` telling its two exit-1 cases apart; the launched
  program's exit status in `results.json`, which would let
  `omn_launches_tool` see the tool's.
