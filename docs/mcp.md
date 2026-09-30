# `omn-mcp` -- a model reads it

`ROADMAP.md` phase 11, landed by the twelfth wave's stream M. An MCP server
over stdio, `mcp/` in the workspace, building the binary `omn-mcp` over the
`omn` library. It exposes what already exists -- the catalog, a run's
resolved file and results document, the capability sweep -- and it is a
second surface on those objects, the way a session file is a second surface
on a `Request`. It is not a report generator: nothing is measured, summarised
or modelled in it, and every number it hands out travels with the fields that
say what that number is missing.

This file is the deliverable, not a log about one: what the server is, how to
run it, the decisions it embodies, how it was checked, and what it does not do.

---

## The walk-through: benchmark a program, and say what dominates its time

The use case this server was built for, asked of an agent by someone who has
never read this repository. Everything below is what
`mcp/tests/walkthrough.rs` does, call by call, against the directory this
section starts the server with; every number quoted was measured by it on the
development machine (24 cpus, kernel 7.2.6, five other streams running).

### 1. The person approves what may run

`examples/mcp/approved/` is a directory of sessions the person starting the
server wrote. Each launches the same program -- `program.py`, a stand-in for
theirs, which computes, sleeps 200 ms and computes again, then says what it
did on one stderr line prefixed `omn-truth` -- and each asks one question:

| Session | Asks | Needs |
|---|---|---|
| `benchmark.yml` | how long the program lived (released to reaped, with the reap's `rusage` cpu), and its cpu poll by poll (`cpu.oncpu_time`) | nothing: `/proc` and `wait4`, every machine serves it |
| `profile.yml` | where its cpu went, by stack (`cpu.profile`, `topk` and `stack`) | a sampling `perf_event_open` with a callchain: withheld at `perf_event_paranoid` 2 and above without `CAP_PERFMON` |
| `decompose.yml` | its life split into on-cpu, runnable and blocked, with the signed residue and each row's resolution | `CAP_BPF` and `CAP_PERFMON` on `omn` (`sched.runqlat`, `sched.offcpu`) |
| `triage.yml` | the triage's cheap pass (`sched.offcpu`, `cpu.oncpu_time`), escalated to the layer the program's time names | the same, and the profile once the cpu layer is named |

To benchmark another program, replace `python3 program.py` in each file with
its command line. The files run from their own directory, so a relative path
is relative to it; nothing in a call can change what they run.

### 2. Start the server

```console
make                     # builds omn, the shim and omn-mcp, and grants omn its capabilities
target/debug/omn-mcp --dir runs --allow-run 20s --allow-launch examples/mcp/approved
```

The client configuration is `examples/mcp/mcp.json` (Claude Code's
`.mcp.json` shape: any client that launches a server by command reads the
same three fields):

```json
{
  "mcpServers": {
    "omn": {
      "command": "/path/to/omniscient/target/debug/omn-mcp",
      "args": [
        "--dir", "/path/to/runs",
        "--allow-run", "20s",
        "--allow-launch", "/path/to/omniscient/examples/mcp/approved"
      ]
    }
  }
}
```

| Flag | Meaning | Default |
|---|---|---|
| `--dir DIR` | where runs are read from (any directory `omn -o` wrote, up to three levels down), and where an allowed run writes, under `DIR/mcp-runs/` | the current directory |
| `--omn PATH` | the `omn` binary that answers every question about this machine | `omn` beside `omn-mcp`, which is where `make` puts it |
| `--allow-run DURATION` | enables `run_session`, each run at most `DURATION` | absent: `run_session` refuses every call and says how it could be enabled |
| `--allow-launch DIR` | with `--allow-run` only: the session files in `DIR` (and the directories under it), which the person starting the server wrote, may launch their programs; `list_approved` lists them and `approved: NAME` names one | absent: no session launches anything. Given without `--allow-run`, the server refuses to start |

### 3. The agent asks what may run: `list_approved`

One entry per file, each with `plan_session`'s answer: the backend that would
serve every request, its rank, its declared cost band, what it degrades, and
whether `run_session` would take it (`through_run_session.allowed`). A
session this machine cannot serve says so here, before anything runs:
`plan.resolution.resolved` is `false`, `whose` is *the machine*, and
`said_by_omn` is the binary's reason, line for line. On a stock kernel at
`perf_event_paranoid` 2 or 4, that is `profile.yml` -- and `decompose.yml`
and `triage.yml` without `CAP_BPF` -- while `benchmark.yml` resolves
everywhere.

### 4. The agent runs one: `run_session`

```json
{"name": "run_session", "arguments": {"approved": "decompose.yml", "duration": "20s", "budget": "25%"}}
```

The duration and budget only bound the run; the server refuses one above its
own `--allow-run`. The answer:

| Field | What it is |
|---|---|
| `exit` | `omn`'s exit status: 0 ran, 1 the machine refused, 2 the request, 3 this build |
| `said_by_omn` | every line `omn` wrote on stderr -- the caveats, and **the program's own stderr**, since the program inherits it. `program.py`'s `omn-truth` line is here |
| `run` | the run's name under `--dir`, for `read_run` |
| `read_back` | the run read back, exactly as `read_run` answers (raw events cut at 100; `read_run` with `max_events: 0` cuts none) |
| `read_back.requests[i].what_it_is_missing` | **the shortfall**, first: non-zero loss counters and their reasons, every `short` mark, the degradations, unverified requirements, filter placement, the worst `ts_err_ns`, why it stopped |
| `read_back.requests[i].result.lifetime` | **the benchmark's number**: `released_ns` to `exited_ns` is the program's life, `exited_err_ns` how late the exit could have been seen, `cpu_ns` the reap's user plus system cpu |
| `read_back.decompositions[0].tables[0]` | **what dominates**: one row per state with its `amount`, `resolution`, `evidence` and `evidence_means`, the life as the whole, and the signed `residue` beside `residue_resolution` |
| `evidence_means`, beside every `evidence` | **the evidence level**, with its rank and what it establishes and does not: `TEMPORAL` says *hypothesis*, `IDENTITY` *propagated and read back* |
| `read_back.requests[i].cost` | what the run cost, declared and measured (`cost.json`) |

Measured, one run each (`the_walk_through_hands_over_what_the_runs_wrote_and_what_the_program_did`):

```
benchmark.yml:  life 599.9 ms (exit within 49.7 ms), the program's own span 588.9 ms, asleep 200.0 ms, on a cpu 395.3 ms by its schedstat
profile.yml:    life 822.5 ms (exit within  1.9 ms), the program's own span 805.9 ms, asleep 200.2 ms, on a cpu 547.1 ms
decompose.yml:  life 741.8 ms (exit within 25.2 ms), the program's own span 726.3 ms, asleep 200.1 ms, on a cpu 524.3 ms
triage.yml:     life 1192.6 ms (exit within 1.4 ms), the program's own span 1180.9 ms, asleep 200.1 ms, on a cpu 984.4 ms
```

For `decompose.yml` the table's biggest row is `on-cpu`, within its stated
resolution (one 50 ms poll) of the program's own `schedstat`; `blocked` holds
the 200 ms sleep; the residue is inside its resolution. For `triage.yml`, the
notes on `blocked` say *the escalation, for latency, named `cpu` at 0.41 s,
attaching `profile`*, and `futex` says it was never attached because `lock`
was never named. **That is the answer to "what dominates its time"**, with the
evidence level of every row beside it.

### 5. Comparing two runs: the spread is the model's to compute, and here is how

Run the same approved session N times; each answer is a whole document with
its own losses and cost. The benchmark's number of run *k* is
`read_back.requests[0].result.lifetime.exited_ns - released_ns`. The spread
the bench's gate computes from N of them (`examples/bench/harness.rs`,
`Stats`, `docs/bench.md` "The variance gate"):

- **median and IQR**, quartiles by linear interpolation (numpy's default);
- **CV**, the sample standard deviation over the mean;
- **resolution** = `3.545 * CV / sqrt(N)`: the smallest difference between
  two such medians that is not noise, at about 95 %. Two runs differ when
  their medians differ by more than it. The bench admits a comparison at a
  resolution of 1 % with at least 20 runs; below 20 it refuses.

Before comparing, read each run's `what_it_is_missing`: a run that lost
events, was killed (`killed`), or ran on a different backend
(`resolved.backend.id`) is not the same measurement, and its number does not
belong in the same median.

`a_repeated_benchmark_has_the_spread_the_bench_would_compute_from_its_files`
runs `benchmark.yml` five times through `run_session`, beside the other test
of the file, and computes the gate's statistics from the answers, from the
files, and from the program's own clock:

```
  WORKLOAD      N   MEDIAN        IQR           CV       RESOLVES   GATE
  handed         5   579.56ms      196.39ms       27.42 %   43.47 %   REFUSE
  files          5   579.56ms      196.39ms       27.42 %   43.47 %   REFUSE
  own clock      5   568.92ms      181.34ms       27.74 %   43.97 %   REFUSE
```

The answers and the files give the same statistics to the bit; each life
holds the program's own span; and on a machine five other agents were loading,
the gate would refuse to compare anything at five runs, which is what an agent
must say too. Why this is N calls and not a tool is below ("Benchmarking is
repeated `run_session` calls").

### Reading runs a person made

Without `--allow-run`, the server reads what `omn` already wrote:

```console
omn -f examples/join-decompose-compute.yml --format json -o runs/decompose
target/debug/omn-mcp --dir runs
```

The protocol is MCP's JSON-RPC 2.0, one message per line. Revisions
`2025-06-18`, `2025-03-26` and `2024-11-05` are spoken; a client asking for
another is answered in the newest. Methods: `initialize`, `ping`,
`tools/list`, `tools/call`, `resources/list`, `resources/templates/list`,
`resources/read`, `prompts/list` (empty). `initialize` returns instructions
for the model that restate what each "missing" field means.

## What it exposes

### Tools

| Tool | Answers | Machine? |
|---|---|---|
| `catalog` | every source, one family or one source, as the catalog declares it: backends in order with requirements, cost band and degradation, modes, emits, targets, keys, filter fields, caveats | no, the library's built-in catalog |
| `capabilities` | `omn list`'s sweep: per source available, unverified or unavailable, and the collector column; with `source`, the full listing with every requirement's probe answer and reason | yes, asked of `omn` |
| `list_runs` | the runs under `--dir`: their requests and backends, why each stopped, which are lossy or carry a shortfall | no, the files |
| `read_run` | one run, regrouped per request (below) | no, the files |
| `plan_session` | a session validated here and resolved by `omn --dump-resolved`: per request the backend that would serve it, its rank in the chain, its cost band, what it degrades, what could not be verified, where its filter would run, and whether `run_session` would take it -- or the rejection, with whose it is | yes, the resolution is asked of `omn` |
| `list_approved` | the session files of `--allow-launch DIR`, each with what `plan_session` says of it; a file that is not one (a link out of the directory) is listed with why | yes, each plan is asked of `omn` |
| `run_session` | one bounded run through `omn`, read back | yes, and only when allowed |

A refusal is a tool result with `isError: true` and the reason as its text,
never a protocol error: it is an answer. Unknown tools and malformed arguments
are protocol errors (`-32602`).

### Resources

| URI | Content |
|---|---|
| `omn://catalog`, `omn://catalog/{source}` | the catalog, as `omn list --format json` prints it |
| `omn://evidence` | the evidence ladder, weakest first, with what each level establishes and does not |
| `omn://capabilities` | the `capabilities` tool's answer |
| `omn://runs` | the `list_runs` answer |
| `omn://runs/{run}/{file}` | `resolved.json`, `results.<ext>` or `cost.json`, byte for byte as `omn` wrote it |

### What `read_run` hands over

Per request, in request order:

- `result`: the entry of `results.json` as written -- outputs with every
  `short` mark, `losses` with their reasons and overrun, `window`,
  `lifetime`, `degradations`, `filtered`, `stopped_because`, `events_seen`.
- `resolved`: the entry of `resolved.json` as written -- backend with its
  requirements and probe answers, the whole chain with a verdict each,
  `degradations`, `unverified`, the filter's placement. The `environment` is
  lifted to the top of the answer once when every request shares it, and left
  in each otherwise.
- `cost`: the entry of `cost.json` as written, declared and measured.
- `what_it_is_missing`: the non-zero loss counters, the loss reasons, every
  `short` mark with the series it is on, the degradations, the unverified
  requirements, the filter placement, the largest `ts_err_ns` among the raw
  events handed over, and why it stopped. Nothing in it is computed; each is a
  field of the result or the resolved request, restated first so that it is
  the first thing read.

Then the run's `joins` and `decompositions` as written, the evidence ladder,
anything else the document carries under its own name (so a field added to the
results after this server was written is passed on, not dropped), and
`missing`: what this answer itself leaves out.

The two additions, and the only two:

- **`evidence_means`** beside every `evidence` field: the level, its rank
  (`1 of 4, weakest first` for `TEMPORAL`, `4 of 4` for `IDENTITY`), what it
  establishes and what it does not. `TEMPORAL` says *hypothesis*; `IDENTITY`
  says *propagated and read back*. They never read alike.
- **`elided`** inside a `raw` output whose events were cut to `max_events`
  (100 by default), with how many the document holds; `missing` gets one line
  per cut. `max_events: 0` hands every event over.

A results file that is a rendering (`results.txt`) is not read into fields:
a number read back out of prose is a number whose shortfall may have been left
behind. The run says so in `missing` and names the resource holding the text.

---

## The decisions

### The server probes nothing; the binary that would run the session does

A probe answers for the process that asks it. `CAP_BPF` and `CAP_PERFMON` are
file capabilities of `target/debug/omn`, and the preload shim is looked for
beside the executable, so the same library linked into `omn-mcp` answers a
different sweep. Measured on the development machine, with a copy of `omn`
that had lost its capabilities and sat away from the shim: **forty-two
sources move** from `yes` to `no` or `?` -- 93 usable became 52 -- the whole
eBPF surface of `sched`, `io`, `net` and `proc`, and the three `mem.heap.*`
whose shim was not beside it, and nothing in the answer says why. That is `CLAUDE.md`'s silent relink failure,
one binary over.

So everything that depends on the machine -- the sweep, the resolution of a
session, a run -- is asked of the `omn` binary, and every such answer carries
`answered_by`: the binary's path and the capabilities read off its
`security.capability` attribute. When the binary lacks one of the two,
`missing` says so and names `make`. The server itself holds no capability and
needs none: a process a model talks to is the last one that should carry
`CAP_BPF`.

Validation is machine-independent (`src/validate.rs` says so), so it runs on
the library, in process, and its rejections come back as fields -- request,
message, hint -- rather than as stderr.

### The sweep is read from the binary's JSON, strictly

Until the twelfth wave's flight-recorder stream, `omn list --format json` was
the catalog without the `HERE` column, and this server read the sweep out of
the text table. It carries the sweep now -- each source's availability and
each backend's requirements with what their probes answered -- and
`mcp/src/binary.rs::parse_sweep` reads that: every row a catalog id in
catalog order, every availability one of the three, the probes passed on as
the binary wrote them. A mismatch is a refusal, never a guess, including a
binary and a server built from different catalogs. The protocol test still
checks the totals against the text table's footer, which is a reference the
server no longer reads.

### `run_session` exists, opt-in, bounded, and launches only what its owner wrote

The brief left open whether a running tool exists at all. It does, under four
conditions, and each is the reason it can exist without breaking boundary 5.

1. **Opt-in, by the person who starts the server.** `--allow-run DURATION`
   and nothing else enables it; absent, every call is refused with the reason.
   This is `make CAPTURE=1`'s shape from `DECISIONS.md`: the one power here
   that is closer to acting than observing is granted only when asked for, by
   the person, once.
2. **Bounded by the server, not the caller.** `duration` and `budget` are
   required. A duration above the server's bound is refused -- the model cannot
   raise it -- and the run is passed `--duration` and `--budget`, which `omn`
   applies to every request and says so. The process is killed at twice the
   duration plus 30 s if it has not ended, and the answer says it was.
3. **A launch only from a session its owner approved.** A session that
   launches a program runs whatever the session names, with this user's
   rights, and a model must never choose what code runs. Until the thirteenth
   wave that meant no launch at all. Since then, the person starting the
   server may pass `--allow-launch DIR` beside `--allow-run`: the session files
   in `DIR` are theirs, and those -- and only those -- may launch. The model
   chooses *which* approved session runs and *when* (`approved: NAME`), never
   *what*: the file is read once, and the text that was checked is the text
   `omn` runs, from the file's own directory, so its program, arguments and
   relative paths are the file's; nothing in a call reaches them (a call
   carries a duration and a budget, which only bound the run). A name is
   refused when it is absolute, holds a `..` or a `.`, or resolves -- through a
   link, the file's or a directory's -- outside `DIR`. A launching session
   given inline or by `path` is refused as before, and the refusal says how to
   allow it. Every other target kind -- a pid, a tid, a cgroup, a cpu set, an
   interface, a container, the system -- names something that already exists
   and runs from anywhere, as before.
4. **It writes only under `--dir/mcp-runs/`,** and reads the run back through
   the same code as `read_run`, so what it answers is what is on disk.

Why not drop it: a model that can only read what a person already ran cannot
check its own hypothesis, and "ask for a measurement" is exactly what the
roadmap grants. Why the launch is allowed now, and only so: benchmarking a
program through an agent is running that program, and the person who wants it
benchmarked is the one who can say which program and how. What boundary 5
protects against is a model choosing the code; a directory of files the
person wrote keeps that choice theirs. The observer effect remains -- a traced
program runs differently, which the bench measured -- but that is true of
every `omn` run and is what `cost.json` reports.

### Benchmarking is repeated `run_session` calls, not a tool of its own

Considered: one tool that runs an approved session N times, bounded and
budgeted, and answers with each measurement's spread, judged the way the
bench judges whether a median resolves (`docs/bench.md`). Not added, for
three reasons.

- **The server does not summarise.** Every answer here is a document `omn`
  wrote, with what it is missing stated first. A spread computed by the
  server is a number no document carries, and each run's own losses, cost and
  degradations would have to be folded into it or dropped. N calls give N
  documents, each read back whole; a model computes the spread from them, and
  can see which run lost events.
- **The bench's verdict needs what the server cannot promise.** Its gate
  refuses a loaded machine and it wants a release binary; `scripts/bench.sh`
  is not exposed, and a tool that borrowed its notion of "resolves" without its
  gate would lend a quiet-machine verdict to a busy one.
- **The bound stays per run.** `--allow-run` bounds one run; a tool of N runs
  would need a second bound nobody asked for, and a stdio server held for N
  times the duration.

What a model does instead: `list_approved`, then `run_session` with
`approved: NAME` as many times as it wants, each answer naming its run so
`read_run` reads it again.

### Verbatim, regrouped, with two documented additions

The server could have shaped its own answer type. It does not, because every
field it chose to leave out would be a place a shortfall could fall through,
and every field it renamed a place a reader's knowledge of the documents stops
applying. It regroups (results, resolved request and cost side by side, per
request, paired by position only when every position names the same source
and label -- otherwise handed over apart and said to be unpaired), restates
what a number is missing at the top of each request, and adds the two keys
above. Nothing else changes, and `mcp/tests/protocol.rs` checks that
key by key against the files.

### Dependencies

`serde_json` for JSON-RPC and the documents, `libc` for `getxattr`, and the
`omn` library. All three were already in the workspace; no crate was added.

---

## How it was checked

`mcp/tests/protocol.rs` is a small MCP client: it spawns `omn-mcp`, sends
`initialize`, `notifications/initialized`, then tool calls and resource reads
line by line, and checks every reply's id. Each test runs a real example
session with `target/debug/omn -f ... --format json -o DIR` first and points
the server at `DIR`. Ground truth is the files `omn` wrote, and, where a
property of the machine is the point, the workload's or the kernel's own
account.

| Test | Workload | What is checked |
|---|---|---|
| `every_number_of_a_run_comes_back_unchanged` | `join-decompose-compute` (six sources, a wakeup join, a decomposition) | every result, resolved request and cost equal to the files after removing the two additions, and the joins and decompositions too; 1346 numbers in the three files; the resources are the files byte for byte |
| `an_answer_cut_short_says_how_short` | `join-between-residency` | `max_events: 2` keeps a verbatim prefix, `elided` counts the document's events, one `missing` line per cut |
| `a_lossy_run_answers_with_its_shortfall` | `cross-compute-lossy-profile` (100 kHz, past `kernel.perf_event_max_sample_rate`) | the run was lossy (on the development machine: 29310 dropped, 45 throttled, 53.5 % unaccounted for); narrowed to one label with its raw events cut to one, the answer still carries the dropped and throttled counts, the reasons, and every `short` mark unchanged; `list_runs` marks it; narrowing says it left the joins out. Skips loudly, with the sample-rate limit, if the profile lost nothing |
| `identity_and_temporal_never_read_alike` | one session per rung: `join-between-residency` (TEMPORAL), `sched-wakeup` (WAKEUP), `cross-memory-which-allocation` (KEY), `vertical-one-request` (IDENTITY) | every `evidence` has an `evidence_means` naming the same level, with exactly `omn::join::Evidence`'s sentences and rank; each run carries the level it was chosen for; no two levels share a rank or a sentence. A rung the machine cannot produce skips loudly by name |
| `a_timestamp_from_another_clock_keeps_its_error` | `runtime-jit` (node writing its perf map; every line known only to within the gap between two reads) | every `ts_err_ns` unchanged, and `timestamp_error.worst_ts_err_ns` is the file's largest (about 8.6 ms here). Skips loudly without node |
| `the_sweep_is_the_one_the_binary_that_runs_gives` | `omn list` | totals, read from `omn list --format json`, equal to the text table's own footer; the capabilities equal to `getcap`'s reading; and the separation: the same binary copied without its capabilities answers fewer available sources, and its `missing` names both capabilities |
| `a_plan_says_what_it_would_serve_and_degrade_and_runs_nothing` | a session whose program would `touch` a marker | the marker does not exist after planning; backend, rank, cost band, degradations and chain equal `omn --dump-resolved`'s; the band says it is a declaration and cites finding 47; `run_session` is announced as refusing it; an invalid session is rejected by field with its label, and the machine is not asked |
| `a_call_that_would_change_the_machine_is_refused` | the same marker, through `run_session` | refused without `--allow-run`; with it, refused for the launch; a duration over the bound and a missing budget refused; run names and resource URIs that climb out of `--dir` refused; no marker, no run directory |
| `only_a_session_its_owner_approved_launches_a_program` | a python program that touches a marker, writes a file relative to where it runs, and says how many descriptors it holds (`FDS 20`) | `--allow-launch` without `--allow-run` refuses to start; without `--allow-launch` the approved session and `list_approved` are refused; with it, the same session inline, by a path outside `DIR`, and by `../outside.yml`, an absolute name, `./counted.yml`, an empty name and a link in `DIR` to a file outside are each refused, and no marker and no run exist; `list_approved` lists the session with its plan (`allowed: true`) and the link as refused; `run_session` with `approved: counted.yml` runs it: the marker exists, the relative file is in `DIR`, and the summary's max is the program's own count, with losses and measured cost |
| `a_bounded_run_over_an_existing_process_reads_back_what_the_process_reports` | a python process holding 17 extra descriptors that counts its own (`FDS 20`) and spins | `run_session` with `proc.fd` on its pid, 1 s, 5 %: the summary's min and max are the process's own count, it stopped because the duration elapsed, the cost is measured; reading the run again gives the files unchanged |

Since the fourteenth wave, `mcp/tests/walkthrough.rs` drives the walk-through
above against a copy of `examples/mcp/approved/` (so a run writes nothing into
the repository), and the protocol tests share their client with it
(`mcp/tests/common/mod.rs`):

| Test | Workload | What is checked |
|---|---|---|
| `the_walk_through_hands_over_what_the_runs_wrote_and_what_the_program_did` | the four approved sessions over `program.py` | `list_approved` lists exactly the four, each allowed and valid, and a session the machine withholds carries the machine's exit and reason (and is skipped out loud); each served session runs through `run_session` (exit 0, not killed), and its read-back **and** `read_run` with `max_events: 0` equal the three files key by key (over 500 numbers in all); every non-zero loss of the file is restated in `what_it_is_missing` and no zero one is; the life handed over holds the program's own span on its own clock; `decompose.yml`'s `blocked` holds the program's sleep, its `on-cpu` is within its stated resolution of the program's `schedstat`, and the row that dominates is the one the program's own figures say; `triage.yml` escalated to the profile, says so, and never attached the lock layer |
| `a_repeated_benchmark_has_the_spread_the_bench_would_compute_from_its_files` | `benchmark.yml`, five times | the bench's own `Stats` (median, quartiles, CV, resolution) from the five answers equal those from the five `results.json` to the bit, for the life and for the reap's cpu; each life holds the program's own span, so the median does too; the resolution is `3.545 * CV / sqrt(N)` |

And the stock machine (the CI's two runs, `perf_event_paranoid` 4 and 2):
`every_number_of_a_run_comes_back_unchanged`, `a_lossy_run_answers_with_its_shortfall`
and the IDENTITY rung of `identity_and_temporal_never_read_alike` skip with
the machine's reason when `omn` refuses their session with exit 1 and *no
viable backend on this machine* (or a `perf_event_open` the kernel refused);
`a_plan_says_what_it_would_serve_and_degrade_and_runs_nothing` no longer
parses an empty `--dump-resolved`: where the machine refuses, it checks that
the plan carries the refusal -- exit 1, *the machine*, and every line the
binary said -- and keeps its machine-independent half. Any other failure
still fails.

And seven unit tests: the ladder is `Evidence`'s own order and serialisation,
an unknown level is said to be unknown rather than guessed at, an existing
annotation is not overwritten, a run name with `..` or an absolute path is
refused, elision counts what it cut, only non-zero losses are hoisted.

The whole suite, once, on this branch: `make check` stopped at
`cross_compute::a_lossy_profile_marks_its_row_and_leaves_the_traced_rows_alone`,
the failure known on `main` (a `cpu.profile` loss count, fixed elsewhere), and
`cargo test --workspace --no-fail-fast` then showed five others, none in
`mcp/`, each passing when rerun alone: `scenario_mem`'s
`every_mode_and_emit_of_the_sampled_sources_is_served_or_refused_by_the_right_code`,
`scenario_net`'s `the_ebpf_leg_of_the_sweep_answers_with_every_emit_it_promised`
and `every_segment_the_kernel_resent_is_one_this_run_saw`, `scenario_sched`'s
`a_workload_that_is_half_blocked_measures_as_half_blocked`, and `vertical`'s
`the_tool_decomposes_each_request_against_its_own_life`. The `cross_compute`
one passed in that run. `cargo clippy` and `cargo fmt --check` are clean, for
the root package and for `omn-mcp` (`make check`'s clippy covers only the
root package; `cargo clippy -p omn-mcp --all-targets -- -D warnings` covers
this one).

---

## What it does not do, and why

- **It does not read a rendered results file into fields** (`results.txt`):
  said above. Run with `--format json`, or read the text as a resource.
- **It does not run two things at once.** Stdio is one conversation; a run
  holds it for the run's duration. The bound makes that a few seconds.
- **The flight recorder (stream R) will need a look.** `run_session` passes
  `--duration`, which today replaces every request's own; a session whose
  duration is *until a predicate fires* is one this server should either
  bound differently or refuse, once R's surface exists. Until then the
  duration override is what bounds it.
- **A relative `path`** in `plan_session` and `run_session` is relative to
  the server's working directory, and a session's own relative paths (a
  workload script) are relative to where `omn` runs, which is the same
  directory. A client that starts the server elsewhere should pass absolute
  paths. An `approved` session is the exception: it runs from its own
  directory.
- **Resources do not notify.** `listChanged` is false; `list_runs` is the way
  to see a run that appeared since.

---

## For the session indexes

The fourteenth wave's sessions for this server are in `examples/mcp/approved/`,
which the index test does not read (it reads `examples/*.yml`), and the walk-through
above is their index. The one top-level session stream J added is the join
engine's (`docs/families/join.md`):

| Session | What it shows |
|---|---|
| `join-overlap-machine.yml` | A thread's waits laid across the machine's memory-pressure readings with no key: `overlap` over state the machine holds once. |

## For `DECISIONS.md`

| Decision | Choice | Reason |
|---|---|---|
| Who answers a machine question asked through MCP | **The `omn` binary that would run the session.** The server probes nothing and holds no capability; every machine-dependent answer names the binary and the capabilities on it. Validation, which is machine-independent, runs on the library. | A probe answers for the process that asks. A copy of `omn` without its capabilities answered 52 usable sources against 93, silently -- the relink failure, one binary over. |
| Whether a model may run a session | **Yes, when the person who started the server passed `--allow-run DURATION`**, for at most that duration, with a required budget, writing only under the server's directory -- and **a session that launches a program only when it is a file of the directory that person passed with `--allow-launch DIR`**, run as the file states it, from its own directory (amended in the thirteenth wave; before it, never). | Asking for a measurement is what phase 11 grants; a model choosing the code that runs is changing the machine (boundary 5). A directory of files the person wrote keeps that choice theirs: the model picks which approved session and when, never what. The opt-in is `make CAPTURE=1`'s shape: the power closest to acting is granted by a person, when asked. |
| Whether benchmarking gets its own tool | **No: repeated `run_session` calls on an approved session.** | The server does not summarise, the bench's verdict needs a quiet machine and a release binary the server cannot promise, and N documents keep each run's losses and cost where a spread would drop them. |
| Where the spread of a repeated benchmark lives (fourteenth wave) | **In the walk-through, not in an answer**: `docs/mcp.md` names the field (`result.lifetime`), the bench's statistics and the gate's two conditions (20 runs, resolution 1 %), and a test holds the answers to the files' statistics. | Reaffirms the thirteenth wave's no-tool decision with the use case run: the answers carry every number the spread needs, unchanged, and each run's losses beside it; a server-computed spread would be the one number no document carries. |
| Where the approved sessions of the walk-through live | **`examples/mcp/approved/`, with the client configuration one level up** (`examples/mcp/mcp.json`). | `list_approved` lists every session-shaped file under the directory, `.json` included: a client configuration inside it would be listed as an unreadable session. |
| What an answer is | **The documents as written, regrouped per request**, with each request's losses, shortfalls, degradations, unverified requirements and timestamp error restated first, and exactly two additions: `evidence_means` beside every evidence level and `elided` where raw events were cut, counted in `missing`. | Every field a shaped answer leaves out is a place a shortfall falls through. `IDENTITY` and `TEMPORAL` are one word each in a document; beside each, what it establishes and does not is what keeps them from reading alike. |

## Filed for another stream

In `docs/open-findings.md`, "Needs from another stream — the twelfth wave,
stream M":

- **Stream R** (`src/main.rs`): `omn list --format json` to carry each
  source's availability and each backend's probes, which retires the text
  parse. Needs `resolve::probe_all` public (V's file).
- **Stream V** (`src/catalog.rs`): `Cost::worst_case_percent`, so the band's
  worst case is not held twice (`main.rs` and `mcp/src/session.rs`).
- **Unowned** (`src/join.rs`): `Evidence::establishes` and
  `does_not_establish`, so the meaning of a level lives beside the level.

In `docs/open-findings.md`, "Needs from another stream — the fourteenth wave,
stream J":

- **Stream P** (`src/catalog.rs`): `Cost::Extreme`'s doc comment to read
  "over 20 %, with no bound above", as `main.rs` renders it and as
  `omn-mcp`'s band now says (R's thirteenth-wave request; the server half is
  landed).
