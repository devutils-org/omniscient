# Omniscient

One vocabulary for system observability. A measurement is described along five
axes — **source**, **target**, **mode**, **emit**, **keys** — and the
combinations that cannot mean anything are refused rather than answered with a
number that means something else.

```
omn io.latency --pid 4242 --mode trace --emit hist,topk:5 \
    --keys blk,task --filter "bytes > 4KiB && rwbs == 'R'" --duration 30s
```

## Status

The spine is complete and one source runs end to end. Request → validation →
machine introspection → backend resolution → collection → aggregation →
encoding, for `mem.rss`:

```console
$ omn mem.rss --emit timeline,summary --mode poll@100ms -- ./a-program
mem.rss  via fs:proc:smaps_rollup  (mem.rss)
  11 events over 1.10s, stopped because process 170997 exited
  no loss: every read succeeded and no tick ran late

  timeline
    task:170997
      +0ns         12.0 KiB
      +100ms       17.6 MiB
      +300ms       25.6 MiB
      ...
```

The other 99 sources resolve but have no collector yet, and say so. That is
deliberate: one source taken all the way down proves the skeleton, and the
skeleton is what every later source is written against.

| Command | What it does |
|---|---|
| `omn <source> [options]` | resolves and runs a request |
| `omn <source> --dump-request` | shows it and how it would be obtained, without measuring |
| `omn <source> --dump-resolved` | the record to keep beside the numbers |
| `omn -f session.yml` | the same, from a file in YAML, JSON or TOML |
| `omn list [family\|source]` | the catalog, marked with what this machine can serve |
| `omn validate -f session.toml` | checks a session without touching the machine |
| `omn formats` | the input and output formats, and what each can carry |

## The five axes

```
source   <family>.<metric>      io.latency, sched.offcpu, net.tcp.rtt
target   what to observe        --pid --tid --cgroup --cpu --iface --container --system -- ./prog
mode     how to observe         count · sample@997hz · trace · poll@100ms · snapshot
emit     what comes out         raw · hist[:log2|:linear:WIDTH] · rate · topk[:N] · stack · timeline · summary
keys     correlation keys       req · task · proc · file · sock · blk · mem · gpu · cgroup
```

`CATALOG.md` is the vocabulary and `catalog/*.yaml` is the same thing as data.
Adding a source means adding an entry: it appears in `omn list`, in validation
and in the help without touching code.

## What it says about this machine

`omn list` is the catalog projected through what was actually probed here:

```console
$ omn list mem
SOURCE          MODES           COST  DET     HERE  SUMMARY
mem.rss         poll, snapshot  free  medium  yes   Resident footprint
mem.cgroup      poll, snapshot  free  medium  no    Cgroup memory accounting
mem.faults      count, sample   free  medium  ?     Minor and major page faults
...
100 sources: 29 usable here, 45 unverified, 26 out of reach on this machine.
```

`?` is not a soft yes. It means no probe has been written for that requirement
class, so nothing was checked — and the resolver will not pick such a backend
when a fully verified one exists. An unchecked assumption that reaches the output
as a fact is the failure mode this column exists to prevent.

The same honesty applies to *why* something is out of reach:

```console
$ omn io.latency --pid 1 --mode trace --emit hist
error: `io.latency` has no viable backend on this machine:
  bpf:tp: tracepoint:block:block_rq_issue: missing: tracefs is mounted but not
    readable by uid 1000 — this needs root or CAP_SYS_ADMIN, not a different kernel
  fs:tracefs: a pid target needs in-program filtering, which `fs:tracefs` cannot do
  fs:proc: a pid target needs in-program filtering, which `fs:proc` cannot do
  hint: the request is valid; it is this machine that cannot serve it.
        Use --no-resolve to write it down for one that can.
```

## What it refuses, and why

```console
$ omn mem.bandwidth --pid 1 --mode count --emit rate
error: `mem.bandwidth` cannot be measured per pid: the counter is scoped to the
       package, so no per-process number exists to return
  hint: this source accepts system
```

Returning a system-wide value while presenting it as per-process attribution is
the worst bug this tool could have. So the validator rejects, before anything is
attached: a target a source cannot carry, a mode it does not support, an emit its
mode cannot produce, a correlation key it does not carry, a filter field it does
not expose, and an output format that cannot express the requested emit.

It also surfaces what is true but easy to miss — that `io.latency --pid` is
reliable for reads only, that `dwarf` unwinding copies the stack on every sample,
that a cold and a warm page cache differ by an order of magnitude — next to the
request, not in documentation nobody reads at the moment it matters.

## Trace as code

A session file describes the same thing a command line does, and
`--dump-request` prints one:

```yaml
name: web latency budget
duration: 30s
budget: 2%
requests:
  - label: device
    source: io.latency
    target: {pid: 4242}
    mode: trace
    emit: [hist, topk:5]
    keys: [blk, task, req]
    filter: "rwbs == 'R' && bytes > 4KiB"
```

YAML, JSON and TOML are all accepted, inferred from the extension or forced with
`--input-format`; `-f -` reads standard input. See `examples/`.

## Output formats

`text` for a terminal, `json` / `yaml` / `toml` for tooling and for the resolved
file, `ndjson` for a record stream. The profile and trace formats are declared
with the emits they can carry — `folded` and `pprof` for stacks, `speedscope` for
stacks and timelines, `chrome` for timelines — and report that their encoder
lands with the first stack-producing collector rather than approximating
something else. `omn formats` lists them.

Every run can write its numbers and, beside them, the resolved request that
produced them:

```console
$ omn mem.rss --pid 4242 -d 30s -e summary --out ./results
$ ls results
resolved.json  results.txt
```

`resolved.json` holds the backend that was selected, every requirement and how it
was probed, what was degraded, and the machine's kernel, governor, THP mode, SMT
state and clocksource. Two numbers taken through different backends are not
comparable, and without that file nobody can tell that they were.

## Boundaries

Stated up front, because a project called "Omniscient" that hides them turns
them into perceived bugs:

- **No bit-exact determinism.** Counters are stable, never exact.
- **No inside view of NVIDIA GPUs.** CUPTI is closed; AMD and Intel are covered.
- **Writes are less attributable than reads.** Writeback loses the submitter.
- **Single machine.** Inter-machine clock drift is a different problem.

## Building

```console
cargo build --release
cargo test
```

`ROADMAP.md` has the milestones and their exit criteria, `DECISIONS.md` what is
settled and what is still open.
