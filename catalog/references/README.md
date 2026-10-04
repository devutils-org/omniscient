# Reference pairs

One file per family of tools, `<family>.yaml`: a list of **pairs**, each
holding one `omn` source against one common use of one established tool, on
the same workload, within a declared tolerance. The tools are witnesses:
`tests/reference.rs` runs them, `omn` never does, and the catalog loader does
not embed these files.

A family is three files that name each other, and `families_are_whole`
fails when one is missing:

- `catalog/references/<family>.yaml` -- the pairs (this schema);
- `tests/reference/<family>.rs` -- `pub fn parsers() -> Vec<Parser>`, each
  parser unit-tested on an output captured under
  `tests/reference/captures/<family>/`;
- `docs/replaces/<family>.md` -- the verdict table, one row per tool and use.

## Running

```console
make                                                      # once: omn and its capabilities
cargo test --test reference pairs_perfstat -- --nocapture # one family
OMN_REFERENCE_PAIR=cpu.instructions cargo test --test reference pairs_perfstat -- --nocapture
cargo test --test reference every_pair_is_well_formed     # the data alone, nothing run
```

Each pair prints one line per outcome:

```text
reference perfstat cpu.instructions / perf stat: same numbers: summary (...): omn 2.53e8 vs perf stat 2.52e8, off by ... where ... is allowed
skipped: reference bpftrace vfs.ops / vfsstat: `sudo -n` does not grant root here, and this tool needs it
gap: reference perfstat cpu.topdown / perf stat: omn has no top-down level 2
```

A pair outside its tolerance, or whose data is wrong, fails the family's
test, naming every such pair at once. A skip is the machine's reason and
nothing else (below).

## A pair

```yaml
- id: instructions-per-process            # optional; unique in the family
  source: cpu.instructions                # the omn source compared
  tool: perf stat                         # the tool, as a person types it
  use: "perf stat -e instructions -- prog"   # the common use this replaces
  workload: [python3, tests/reference/workloads/perfstat/spin.py, "2"]
  omn: [cpu.instructions, -m, count, -e, summary, --format, json]
  run: separate                           # or attach (below)
  runs: 1                                 # per side; medians are compared
  timeout_s: 120                          # per command
  reference:
    command: [perf, stat, -x, ",", -e, instructions, --]
    report: []                            # optional second command, parsed instead
    parse: perf_stat_csv                  # a parser's name in the family's module
    requires: [perf]                      # binaries (or paths); missing skips
    privileged: false                     # true: run through `sudo -n`
    refused_when: ["not supported"]       # stderr text meaning the machine refused
  compare:                                # one, or a list
    key: ""                               # the parsed value's key; "*" for each
    omn: results[source=cpu.instructions].outputs[emit=summary].series.*.mean
    tolerance: {rel: 0.02}                # and/or {abs: N}; the larger is allowed
    why: "both read the same PMU counter over the same process; multiplexing off"
    omn_scale: 1                          # multiplies omn's value (ns -> us: 0.001)
    reference_scale: 1                    # multiplies the tool's
  gap: "..."                              # instead of running: what omn lacks
```

Every field the runner does not know is ignored, so a family may add its own
notes. Relative paths resolve from the repository's root, which is every
command's working directory.

### How it runs: `run`

- **`separate`** (the default): `omn <omn> -- <workload>`, then
  `<command> <workload>`, two runs. With `runs: N` they alternate (`omn`
  first on even runs, the tool first on odd ones) and the medians are
  compared. Say so in `why`: on a machine twelve streams load, two separate
  runs differ by the load.
- **`attach`**: one run seen by both. The runner launches the workload
  stopped before it execs, starts `omn <omn>` and `<command>` with `{pid}`
  replaced by the workload's pid (no workload appended to either), waits
  `settle_ms` (500 by default) for both to attach, then lets it go. Both
  observers must end on their own: by following the pid (`perf stat -p`,
  `omn --pid` with a `--duration`) or by a duration or a count of their own
  (`vmstat 1 3`). A system-wide pair uses `attach` too, with no `{pid}`: the
  workload is what both watch the machine through.

### Placeholders

In `omn`, `command` and `report`: `{tmp}` is the run's scratch directory
(`perf record -o {tmp}/perf.data`, then `perf report -i {tmp}/perf.data`),
`{pid}` the workload under `attach`, and `{out}` the directory for
`omn -o {out}` -- whose `results.json` is then read instead of `omn`'s
stdout. Use `-o {out}` whenever the workload prints anything: `omn`'s JSON is
one document, and a launched program's stdout lands inside it.

### Where the value is: the `omn` path

Dot-separated, into `omn`'s JSON (or its NDJSON, read as an array):

| Segment | Means |
|---|---|
| `results` | an object's field |
| `0` | an array's element (or an object's field named `0`) |
| `*` | every element (or every value of an object) |
| `[source=cpu.instructions]` | every element whose field equals the text |
| `{key}` | the compared key, substituted first (`key: "*"`) |

Where a `*` or a filter matches several numbers, their **sum** is the value:
`series.*.mean` adds every thread's count. A path that finds nothing, or a
value that is not a number, fails the pair.

### Keys

A parser returns `(key, value)` pairs; `""` is a scalar. `key: "*"` compares
every key the tool gave (each bucket of a histogram, each tid), with
`{key}` in the `omn` path; a key the tool gave and `omn` did not fails the
pair. Name the keys so the two sides can agree: a histogram's bucket by its
lower bound in the unit `omn` reports, a thread by its tid.

### Skips: the machine's reason, and nothing else

| The pair skips when | The line says |
|---|---|
| a binary in `requires` is not installed | `` `<binary>` is not installed here `` |
| `privileged: true` and `sudo -n true` fails | `` `sudo -n` does not grant root here `` |
| the tool exits non-zero with a refusal on stderr (`perf_event_paranoid`, `Permission denied`, `Operation not permitted`, `must be run as root`, `requires root`, or the pair's `refused_when`) | `` `<tool>` was refused on this machine ``, quoting those lines |
| `omn` exits 1 (this machine cannot serve it) | `omn cannot run this here`, quoting `omn`'s stderr |

`omn` exiting 2 (the request means nothing) fails the pair; exiting 3
(nothing in this build collects it) fails it too, because that is a gap the
pair should declare with `gap:`. The skip lines are what
`.github/expected-skips.txt` matches on the CI's cells, which install none of
the reference tools.

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
