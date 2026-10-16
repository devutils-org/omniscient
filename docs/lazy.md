# The lazy mode: a family and a target, nothing else

`omn` can describe almost any measurement, and that is what makes it heavy
the first time: a source, a mode, an emit, a stack mode and a duration before
the first number. The lazy mode asks for two things only -- **what you want
to know about** (a family) and **what to look at** (a program or a pid) --
and composes the rest.

```console
omn cpu -- ./prog              # where does the program spend its cpu time?
omn mem -- ./prog              # how much memory does it hold, and does it grow?
omn io -- ./prog               # how long does it wait on its reads and writes?
omn net -- ./prog              # how long does it wait on its sockets?
omn sched -- ./prog            # how often is it switched out, how long off a cpu?
omn proc -- ./prog             # how many threads and open files does it hold?
omn gpu -- ./prog              # how busy is the GPU, how much memory does it hold?
omn power --system -d 5s       # how much energy does the machine draw?
omn mem --pid 1234 -d 10s      # the same questions, about a running process
```

`omn cpu` is short for `omn trace cpu`, exactly as `omn cpu.profile` is short
for `omn trace cpu.profile`.

## What it prints first

Before anything is measured, the run says what it chose for you, and how you
would have asked for it yourself:

```console
$ omn cpu -- ./prog
note: `omn trace cpu` asks the family's first question -- "Where does the program spend its CPU time?" -- with its `low` default, the cheapest it has (`--cost med` or `--cost high` asks for more)
note: composed, in the long form: omn trace cpu.profile --mode sample@99hz --emit stack -- ./prog
```

That second line is the long form. Copy it, change one flag, and you are using
`omn` the full way: `omn list cpu.profile` says what else that source can do.

When a default holds several requests, they run together over the same
window, and `--dump-request --format yaml` writes them as a session file you
can keep and run with `omn -f`.

## How much you are ready to pay: `--cost`

Every measurement costs the program something. The catalog says, for every
way of measuring, roughly how much: `low` is under 1 % of the program's own
cpu time, `med` 1 to 5 %, `high` 5 to 20 %. `--cost` picks how much of the
answer you want at that price:

```console
omn cpu --cost low -- ./prog    # a profile at 99 samples a second
omn cpu --cost med -- ./prog    # at 997 a second: ten times the detail
omn cpu --cost high -- ./prog   # and every caller, unwound with DWARF
```

Without `--cost`, the cheapest band the family has. A family that has nothing
at the band you ask uses the one under it and says so; one whose cheapest
default costs more than you asked refuses and tells you which band to ask
(`io` starts at `med`: following a program's reads and writes means tracing
them).

The band chooses *what* is measured, and *how* where the source has a choice:
a source whose first way on this machine costs more than the band is served
by the first later one that keeps it, and the run says which and what it
gives up (`mem.rss` at `--cost low` reads `statm`, resident pages only,
rather than `smaps_rollup`). If no way it has keeps the band, the run says so
before it starts, and the `cost:` line at the end says what it actually
spent. To hold a run to a measured ceiling, add `--budget 2%`: the run then
moves to a cheaper way, or stops, when the measurement says it is over.

## What it will not do

- **Guess a language runtime.** `omn runtime` refuses: a garbage collector, a
  JIT and an allocator are each one runtime's, and which one a program runs is
  not known before it runs. The refusal lists the sources to pick from.
- **Measure a program with a machine-wide family.** Every `power` source reads
  the whole machine, so `omn power` runs on `--system` for a `--duration`;
  start the program beside it.
- **Mix in your own flags.** `--mode`, `--emit`, `--keys`, `--filter`,
  `--stack` and the others describe one request, and a default composes its
  own. Name the source instead, starting from the long form it printed.

Nothing is hidden by the convenience: what the run degraded, what it lost and
what it cost is said exactly as on any other run.

## Where the defaults come from

Each family's default is data in its own catalog file, `catalog/<family>.yaml`,
the `- default:` item at the end. `omn list` shows them all after the table,
and `omn list cpu` shows one:

```console
$ omn list cpu
...
DEFAULTS: what `omn trace <family> -- PROGRAM` composes, cheapest band first; `--cost low|med|high` picks the band (docs/lazy.md).
cpu      "Where does the program spend its CPU time?"
  low    cpu.profile -m sample@99hz -e stack
  med    cpu.profile -m sample@997hz -e stack
  high   cpu.profile -m sample@997hz -e stack --stack dwarf
```

`omn list --format json` carries the same: each source's `defaults`, the
band and the request its family's default writes there, beside each
backend's `emits`. `omn-mcp`'s `plan_session` and `run_session` take
`family` and `cost` (and `pid`, `cgroup`, `system`, `duration`) in place of a
session, compose it as the command does, and answer what they decided under
`composed`.

A default is held to what a request would be: the catalog refuses to load one
that names a source, a mode, an emit or a key its family does not have, or
puts a source in a band its cheapest backend cannot keep. The reasoning is in
`DECISIONS.md`, "the lazy mode".
