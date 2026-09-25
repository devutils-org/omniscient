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

## Running it

```console
make                                   # builds omn, the shim and omn-mcp, and grants omn its capabilities
omn -f examples/join-decompose-compute.yml --format json -o runs/decompose
target/debug/omn-mcp --dir runs        # speaks MCP on stdin/stdout
```

A client configuration, for any MCP client that launches a server by command
(Claude Code's `.mcp.json` shown):

```json
{
  "mcpServers": {
    "omn": {
      "command": "/path/to/omniscient/target/debug/omn-mcp",
      "args": ["--dir", "/path/to/runs"]
    }
  }
}
```

| Flag | Meaning | Default |
|---|---|---|
| `--dir DIR` | where runs are read from (any directory `omn -o` wrote, up to three levels down), and where an allowed run writes, under `DIR/mcp-runs/` | the current directory |
| `--omn PATH` | the `omn` binary that answers every question about this machine | `omn` beside `omn-mcp`, which is where `make` puts it |
| `--allow-run DURATION` | enables `run_session`, each run at most `DURATION` | absent: `run_session` refuses every call and says how it could be enabled |

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

### The sweep is read from text, strictly, until the JSON carries it

`omn list --format json` is the catalog without the `HERE` column; the sweep
exists only in the text table. `mcp/src/binary.rs::parse_sweep` reads it under
checks that make a drift loud rather than wrong: the header names the columns,
every row is a catalog id in catalog order, every `HERE` is `yes`, `?` or `no`,
and the tallies equal the footer the binary printed. A mismatch is a refusal,
never a guess -- including a binary and a server built from different
catalogs. The JSON that would retire the parse is filed for stream R.

### `run_session` exists, opt-in, bounded, and never launches

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
3. **Never a launch target.** A session that launches a program runs whatever
   the session names, with this user's rights. That is changing the machine,
   not observing it, and it is the one thing a model must not be able to do
   through this surface. Every other target kind -- a pid, a tid, a cgroup, a
   cpu set, an interface, a container, the system -- names something that
   already exists, and observing it is what `omn` does for a person. A launch
   is for a person to run with `omn -f`; `plan_session` says ahead of time
   which sessions `run_session` would refuse.
4. **It writes only under `--dir/mcp-runs/`,** and reads the run back through
   the same code as `read_run`, so what it answers is what is on disk.

Why not drop it: a model that can only read what a person already ran cannot
check its own hypothesis, and "ask for a measurement" is exactly what the
roadmap grants. Why not let it launch: because "it reads; it does not run" is
about the machine, and a launch is the machine running code a model chose. The
observer effect remains -- a traced program runs differently, which the bench
measured -- but that is true of every `omn` run and is what `cost.json`
reports.

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
| `identity_and_temporal_never_read_alike` | `join-between-residency` (`between`, TEMPORAL) and `vertical-one-request` (IDENTITY rows) | every `evidence` has an `evidence_means` naming the same level; TEMPORAL is rank 1 and says hypothesis, IDENTITY rank 4, and no field of the two is equal. The IDENTITY half skips loudly without the vertical server `make` builds |
| `a_timestamp_from_another_clock_keeps_its_error` | `runtime-jit` (node writing its perf map; every line known only to within the gap between two reads) | every `ts_err_ns` unchanged, and `timestamp_error.worst_ts_err_ns` is the file's largest (about 8.6 ms here). Skips loudly without node |
| `the_sweep_is_the_one_the_binary_that_runs_gives` | `omn list` | totals equal to the binary's own footer; the capabilities equal to `getcap`'s reading; and the separation: the same binary copied without its capabilities answers fewer available sources, and its `missing` names both capabilities |
| `a_plan_says_what_it_would_serve_and_degrade_and_runs_nothing` | a session whose program would `touch` a marker | the marker does not exist after planning; backend, rank, cost band, degradations and chain equal `omn --dump-resolved`'s; the band says it is a declaration and cites finding 47; `run_session` is announced as refusing it; an invalid session is rejected by field with its label, and the machine is not asked |
| `a_call_that_would_change_the_machine_is_refused` | the same marker, through `run_session` | refused without `--allow-run`; with it, refused for the launch; a duration over the bound and a missing budget refused; run names and resource URIs that climb out of `--dir` refused; no marker, no run directory |
| `a_bounded_run_over_an_existing_process_reads_back_what_the_process_reports` | a python process holding 17 extra descriptors that counts its own (`FDS 20`) and spins | `run_session` with `proc.fd` on its pid, 1 s, 5 %: the summary's min and max are the process's own count, it stopped because the duration elapsed, the cost is measured; reading the run again gives the files unchanged |

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
  paths.
- **Resources do not notify.** `listChanged` is false; `list_runs` is the way
  to see a run that appeared since.

---

## For the session indexes

No session was added: the server reads what the existing sessions write, and
its tests run `join-decompose-compute`, `join-between-residency`,
`cross-compute-lossy-profile`, `vertical-one-request` and `runtime-jit` as
they are. No row for `examples/README.md` or `GUIDE.md`.

## For `DECISIONS.md`

| Decision | Choice | Reason |
|---|---|---|
| Who answers a machine question asked through MCP | **The `omn` binary that would run the session.** The server probes nothing and holds no capability; every machine-dependent answer names the binary and the capabilities on it. Validation, which is machine-independent, runs on the library. | A probe answers for the process that asks. A copy of `omn` without its capabilities answered 52 usable sources against 93, silently -- the relink failure, one binary over. |
| Whether a model may run a session | **Yes, when the person who started the server passed `--allow-run DURATION`**, for at most that duration, with a required budget, writing only under the server's directory -- and **never a session that launches a program**. | Asking for a measurement is what phase 11 grants; running a program a model chose is changing the machine (boundary 5). The opt-in is `make CAPTURE=1`'s shape: the power closest to acting is granted by a person, when asked. |
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
