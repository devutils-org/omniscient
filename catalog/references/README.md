# Reference pairs

One file per family of tools, `<family>.yaml`: a list of **pairs**, each
holding one `omn` source against one common use of one established tool, on
the same workload, within a declared tolerance. The tools are witnesses:
`tests/reference.rs` runs them, `omn` never does, and the catalog loader does
not embed these files.

A family is three files that name each other, and `families_are_whole`
fails when one is missing:

- `catalog/references/<family>.yaml` -- the pairs (this schema);
- `tests/reference/<family>.rs` -- `pub fn parsers() -> Vec<Parser>` (the tools' and, for `omn_parse`, `omn`'s), each
  parser unit-tested on an output captured under
  `tests/reference/captures/<family>/`;
- `docs/replaces/<family>.md` -- the verdict table, one row per tool and use.

## Running

```console
make                                                      # once: omn and its capabilities
cargo test --test reference pairs_perfstat -- --nocapture # one family
OMN_REFERENCE_PAIR=cpu.instructions cargo test --test reference pairs_perfstat -- --nocapture
cargo test --test reference every_pair_is_well_formed     # the data alone, nothing run
cargo test --test reference reference_tally -- --ignored --nocapture  # after pairs_*: every family's tally
```

Each pair prints one line per outcome, and each family ends on its tally:

```text
reference perfstat cpu.instructions / perf stat: same numbers: results[...].mean: omn 253012345 vs perf stat 252987654, off by +24691.0000 where 5059753.0800 is allowed
skipped: reference bpftrace vfs.ops / vfsstat: `sudo -n` does not grant root here for `bpftrace`, and this tool needs it
gap: reference perfstat cpu.topdown / perf stat: omn has no top-down level 2
known defect: reference sysstat cpu.util / vmstat: open-findings 61: ...
    cpu.util (...): omn 41.2 vs vmstat 50.0, off by -8.8000 where 2.5000 is allowed -- ...
tally: reference perfstat: 9 held, 2 skipped, 1 gaps, 0 known defects, 0 failed
```

A pair outside its tolerance, or whose data is wrong, fails the family's
test, naming every such pair at once. A skip is the machine's reason and
nothing else (below). The tally of a run with no `OMN_REFERENCE_PAIR` is
also left where `reference_tally` reads it, so a coordinator who ran every
family reads one table across them.

## A pair

```yaml
- id: instructions-per-process            # optional; unique in the family
  source: cpu.instructions                # the omn source compared
  tool: perf stat                         # the tool, as a person types it
  use: "perf stat -e instructions -- prog"   # the common use this replaces
  workload: [python3, tests/reference/workloads/perfstat/spin.py, "2"]
  omn: [cpu.instructions, -m, count, -e, summary, --format, json]
  run: separate                           # how the two sides run (below)
  runs: 1                                 # runs; medians are compared
  settle_ms: 500                          # attach, alongside: the observers' head start
  timeout_s: 120                          # per command
  setup: [sh, -c, "..."]                  # optional: an argv run before each side
  reference:
    command: [perf, stat, -x, ",", -e, instructions, --]
    report: []                            # optional second command, parsed instead
    parse: perf_stat_csv                  # a parser's name in the family's module
    requires: [perf]                      # binaries (or paths); missing skips
    requires_setting:                     # files holding an integer, and its bound
      - {file: /proc/sys/kernel/perf_event_paranoid, at_most: 0}
    privileged: false                     # true: run through `sudo -n`
    refused_when: ["not supported"]       # stderr text meaning the machine refused
  compare:                                # one, or a list
    key: ""                               # the parsed value's key; "*" for each
    omn: results[source=cpu.instructions].outputs[emit=summary].series.*.mean
    # omn_parse: omn_summary              # or: a parser of omn's output (below)
    tolerance: {rel: 0.02}                # below: rel, abs, side, log2_buckets
    why: "both read the same PMU counter over the same process; multiplexing off"
    omn_scale: 1                          # multiplies omn's value (ns -> us: 0.001)
    reference_scale: 1                    # multiplies the tool's
  known_defect: "..."                     # optional: omn is wrong here, the finding
  gap: "..."                              # instead of running: what omn lacks
```

**A field the runner does not read is refused**, by
`every_pair_is_well_formed` and before the pair runs: a pair that says more
than the runner does would claim a check nobody makes. A family that needs
a field the runner lacks files it with the runner's owner and declares the
pair `gap:` with the reason meanwhile. Prose goes in `why`, `use`, `gap` or
a YAML comment. Relative paths resolve from the repository's root, which is
every command's working directory.

### How it runs: `run`

Five modes. `separate` runs each side on its own launch of the workload;
the four others are **one run** both sides see, which is what a value that
differs between two runs needs (a rate, a share of the machine, a sampled
latency).

| `run` | What runs | For |
|---|---|---|
| `separate` (default) | `omn <omn> -- <workload>`, then `<command> <workload>` | what the workload fixes: counts, sizes, a deterministic total |
| `attach` | the workload started stopped; `omn <omn>` and `<command>` with `{pid}`; then the workload let go | a tool that follows a pid (`perf stat -p`, `pmap`, `top -p`) |
| `alongside` | `omn <omn>` and `<command>`, nothing appended; after `settle_ms` the workload | a system-wide view over the same seconds (`vmstat 1 3`, `ip -s link`, `execsnoop`) |
| `tool_wraps_omn` | `<command> omn <omn> -- <workload>` | a tool that launches its program (`perf stat --`, `turbostat`): its numbers over omn and the program, omn's over the program |
| `omn_launches_tool` | `omn <omn> -- <command> <workload>` | a tool that is itself the work, or launches it (`fio`, `ncu`, `nsys`): omn's numbers over the tool and the program |

- **`separate`**: two runs. With `runs: N` they alternate (`omn` first on
  even runs, the tool first on odd ones) and the medians are compared. Say
  so in `why`: on a loaded machine, two separate runs differ by the load.
- **`attach`**: the runner launches the workload stopped before it execs,
  starts both observers with `{pid}` replaced by the workload's pid (no
  workload appended to either), waits `settle_ms` (500 by default) for both
  to attach, then lets it go. Both observers must end on their own: by
  following the pid (`perf stat -p`, `omn --pid` with a `--duration`) or by
  a duration or a count of their own.
- **`alongside`** is `attach` with no `{pid}` -- a name, not a mechanism of
  its own: the same start-up, the same settle, the same wait, and `{pid}`
  refused. Both observers watch the machine, so both end on their own
  duration (`omn ... --system -d 3s`, `vmstat 1 3`); the workload, when
  there is one, is what they watch the machine through. With no workload,
  both simply run side by side.
- **`tool_wraps_omn`**: one process tree. `omn` needs `-o {out}` (refused
  without it): the tool's stdout and omn's are one stream, so omn's
  results are read from `{out}/results.json`, and the tool's parser reads
  the command's whole stdout and stderr, omn's human summary included. The
  tool's exit status is the command's, so a refusal is read first, then
  omn's exit. **A tool that counts through inherited perf events counts
  nothing past `omn`'s exec** while `omn` carries its file capabilities
  (`make` grants them): the kernel ends a task's inherited perf events when
  it execs a binary that is not dumpable. Shown 2026-10-04: `perf stat -e
  instructions:u -- omn ... -- python3 ...` reads 0 over the capable `omn`
  and 963 480 494 over a copy without capabilities. Such a pair reads 0
  rather than skipping; give it `omn_launches_tool` or `separate`, or wrap
  a tool that does not inherit counters (`turbostat`, RAPL's energy).
- **`omn_launches_tool`**: the program `omn` launches is a shell that
  `exec`s the tool, so the pid `omn` watches is the tool's; the tool's
  stdout and stderr go to two files of the run, which are what its parser
  reads, and never into omn's output. `omn` reaps the tool and exits 0
  whatever the tool did, so the tool's exit status is read through a
  pidfd: the shell writes its pid and waits on a fifo until the runner
  holds a pidfd on it, and once `omn` has ended the pidfd says how the task
  exited (`PIDFD_INFO_EXIT`, Linux 6.15). A non-zero status skips on a
  refusal wording, as below, and otherwise fails the pair, even when the
  output parses. Where the status cannot be read, the pair skips. Under
  `privileged: true` the shell `exec`s `sudo -n <command>`, so omn watches
  `sudo` and the tool is its child.

`runs: N` repeats any mode N times and compares medians. **`setup`** is an
argv run before each side under `separate` and before the one run in the
other modes, with the placeholders below; it must exit 0, or the pair fails
quoting its stderr. It is for state the workload needs and does not make
itself (a file, a directory); a process that must outlive it belongs in the
workload.

### Placeholders

In `omn`, `command`, `report` and `setup`: `{tmp}` is the run's scratch
directory (`perf record -o {tmp}/perf.data`, then `perf report -i
{tmp}/perf.data`), `{pid}` the workload under `attach` (and nowhere else),
and `{out}` the directory for `omn -o {out}` -- whose `results.json` is then
read instead of `omn`'s stdout. Use `-o {out}` whenever the workload prints
anything: `omn`'s JSON is one document, and a launched program's stdout
lands inside it. Nothing is substituted in `workload`, and a placeholder
there is refused.

### Where the value is: `omn` (a path) or `omn_parse` (a parser)

Each compare names `omn`'s side one of two ways, never both.

**`omn: <path>`**, for a value `omn` holds at one place. Dot-separated, into
`omn`'s JSON (or its NDJSON, read as an array):

| Segment | Means |
|---|---|
| `results` | an object's field |
| `0` | an array's element (or an object's field named `0`) |
| `*` | every element (or every value of an object) |
| `[source=cpu.instructions]` | every element whose field equals the text |
| `{key}` | the compared key, substituted first (`key: "*"`) |

Where a `*` or a filter matches several numbers, their **sum** is the value:
`series.*.mean` adds every thread's count. A path that finds nothing, or a
value that is not a number, fails the pair. That is the whole grammar: no
`[*]`, no `[0]` (write `.0`), no arithmetic, no jq.

**`omn_parse: <name>`**, for anything else: a ratio of two fields, a
weighted mean over series, a value read off a histogram. `<name>` is a
`Parser` in the family's module, beside the tool's, unit-tested on an `omn`
output captured under `tests/reference/captures/<family>/` like the tool's.
It gets `omn`'s output -- `{out}/results.json` when `omn` has `-o {out}`,
its stdout otherwise -- as `stdout`, and `omn`'s stderr as `stderr`, and
returns keyed values; each compared key is looked up in them exactly as in
the tool's. `omn`'s output need not then be JSON at all.

### Keys

A parser returns `(key, value)` pairs; `""` is a scalar. `key: "*"` compares
every key the tool gave (each bucket of a histogram, each tid), with
`{key}` in the `omn` path, or looked up in the `omn_parse` parser's values;
a key the tool gave and `omn` did not fails the pair. Name the keys so the
two sides can agree: a histogram's bucket by its lower bound in the unit
`omn` reports, a thread by its tid.

### Tolerances

| `tolerance` | Holds when |
|---|---|
| `{rel: 0.02}` | `|omn - tool| <= 0.02 * |tool|` |
| `{abs: 5}` | `|omn - tool| <= 5` |
| `{rel: 0.02, abs: 5}` | the larger of the two bands |
| `{rel: 0.05, side: above}` | `0 <= omn - tool <= band`: omn may exceed the tool, never fall short (other readers only add) |
| `{rel: 0.05, side: below}` | `-band <= omn - tool <= 0`: omn may fall short, never exceed (omn's span inside the tool's) |
| `{log2_buckets: 1}` | `floor(log2(omn))` and `floor(log2(tool))` at most 1 apart; a value not above 0 is in no bucket, and equals only another such |

`log2_buckets` stands alone (no `rel`, `abs` or `side` beside it): it is for
a value both sides read off a log2 histogram, which each knows only to a
bucket, and whose bucket edges differ by a unit (bpftrace's microseconds,
omn's nanoseconds). Scale both to one unit first (`omn_scale`,
`reference_scale`, or the parser).

**A tolerance is widened only by a mechanism written in `why`** -- two
separate runs, a poll's window, a sampling rate -- never to make a pair
hold. A pair whose numbers differ because `omn` is wrong is a known defect.

### Known defects

`known_defect: "<finding>"` (where the finding is written down:
`docs/open-findings.md` or the family's `docs/replaces/<family>.md`) runs the
pair as usual. Outside its tolerance, it prints `known defect:` with the
finding and the numbers, and the pair counts as a known defect, not a
failure. **Inside its tolerance it fails**: "the defect no longer
reproduces: remove `known_defect`", so the marker cannot outlive the defect.
A path that finds nothing or a key a side lacks still fails, known defect
or not. `known_defect` and `gap` do not go together.

### Skips: the machine's reason, and nothing else

| The pair skips when | The line says |
|---|---|
| a binary in `requires` is not installed | `` `<binary>` is not installed here `` |
| a `requires_setting` file's value is past its bound | `` `<file>` is <value> here, and this pair needs it at most <bound> `` (or `at least`) |
| a `requires_setting` file cannot be read | `` `<file>` cannot be read here (<error>), and this pair needs it `` |
| `privileged: true` and `sudo -n -l <tool>` fails (the tool itself is asked, since a sudoers file may grant the witnesses one by one) | `` `sudo -n` does not grant root here for `<tool>` `` |
| the tool exits non-zero with a refusal on stderr (`perf_event_paranoid`, `Permission denied`, `Operation not permitted`, `must be run as root`, `requires root`, or the pair's `refused_when`) | `` `<tool>` was refused on this machine ``, quoting those lines |
| `omn` exits 1 (this machine cannot serve it) | `omn cannot run this here`, quoting `omn`'s stderr |
| `omn_launches_tool` and the kernel cannot tell how the tool exited (before Linux 6.15) | `` the exit status of `<tool>` launched by omn cannot be seen here ``, quoting why |

Checked in that order, `requires` and `requires_setting` before anything
runs. A `requires_setting` file holding something other than an integer
fails the pair. Before a refusal or an `omn` exit 1 is granted as a skip,
the workload runs once alone: a workload that fails alone fails the pair.
`omn` exiting 2 (the request means nothing) fails the pair; exiting 3
(nothing in this build collects it) fails it too, because that is a gap the
pair should declare with `gap:`. The skip lines are what
`.github/expected-skips.txt` matches on the CI's cells, which install none
of the reference tools.

### Outcomes and the tally

| Outcome | Line | Counts as |
|---|---|---|
| every compare within tolerance | `reference <family> <label>: same numbers: ...` | held |
| the machine's reason | `skipped: reference <family> <label>: ...` | skipped |
| `gap:` | `gap: reference <family> <label>: ...` | gaps |
| `known_defect:`, still outside | `known defect: reference <family> <label>: ...` | known defects |
| anything else, or a pair that is not well-formed | in the test's panic | failed |

`pairs_<family>` ends on `tally: reference <family>: H held, S skipped, G
gaps, K known defects, F failed`, and fails when F is not 0.

## The tables: `docs/replaces/<family>.md`

One row per tool and use, columns: the tool and its use, the `omn` command
that replaces it, the share of that use covered, same numbers (yes / within
N / no), what is missing. `every_row_names_a_pair_or_why_none` requires each
row to be backed:

- by a pair's `id` in backquotes anywhere in the row, or
- by a pair whose `source` appears anywhere in the row **and** whose `tool`
  appears in the row's first cell, or
- by the words `no pair`, then why: `no pair: omn has no top-down level 2`.

`docs/replaces.md` is the index across families and the summary of what
`omn` replaces fully, in part, and not at all.
