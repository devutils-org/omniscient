# The flight recorder

Stream R of the twelfth wave: `STATE.md`'s "What remains", item 1, as
`DECISIONS.md` settled it -- *a session whose duration is until a predicate
fires, whose buffers are the bounded rings and maps it already has, and whose
output is one document per trigger*. A trigger and a retention bound, not a new
kind of program. Also here: stream E's `collect::Unserved`, stream M's JSON
sweep, and the `stack` emit's refusal the coordinator traced to the
`mem.latency` flap.

## What it is

A session with a `record:` block:

```yaml
duration: 30s                 # the longest the recording may run
requests:
  - label: descriptors
    source: proc.fd
    target: {launch: [./prog]}
    mode: poll@20ms
    emit: [summary, timeline]
record:
  trigger: "value >= 150"     # the filter grammar, over one round of `on`
  on: descriptors             # may be left out when there is one request
  retain: 1s                  # how far back a document reaches
  after: 600ms                # how long it goes on after the trigger (0 if absent)
  documents: 5                # the most it writes; the run ends with the last
```

or on the command line, on its one request:

```console
omn proc.fd -m poll@20ms -e summary -d 1h --until 'value >= 150' \
    --retain 1s --documents 5 -o /tmp/rec --pid 4242
```

The run is the ordinary one -- same collectors, same rounds, same budget --
with three differences:

1. **It keeps only what the retention holds.** Every request's events older
   than `retain` are evicted from the buffers the rounds already read into, and
   counted. Nothing new is kept in the kernel.
2. **After each round of the `on` request, the trigger reads that round's
   events.** It fires on the round in which it *becomes* true -- a round with a
   matching event after a round with none -- so a level held for a second
   writes one document, not fifty.
3. **When it fires, the run goes on for `after`, then cuts a document and goes
   on recording.** The document is the window from `retain` before the trigger
   to the round it was cut at: every request's events inside it, aggregated by
   the same `emit::aggregate` as any run, with its joins, beside the trigger
   record -- the predicate, the request, `fired_ns` (the round's
   `CLOCK_MONOTONIC`), `event_ts_ns` (the event that made it true), the value it
   fired with and every predicate field as that event carried it, the window,
   and what was evicted since the document before it.

It ends at its `documents`, at its `duration`, or when a launched program exits;
a document still open then is cut short and says so (`cut_short`).

### Where the documents go

With `-o DIR`: `DIR/trigger-<n>.<ext>`, written the moment each is cut -- a
recorder whose documents waited for the end of the run would lose them all to a
crash, which is when they are wanted -- and at the end `resolved.json`,
`cost.json` and `recording.json`, the record of the whole recording: every
trigger, why the run ended, the rolling budget span, and what was read after
the last document and is in none (`unrecorded`). Without `-o` the documents go
to stdout one after another, which only `text` and `ndjson` can carry; JSON,
YAML, TOML and the profile formats are refused there with the reason. JSON,
YAML and TOML documents carry the trigger inline as a `trigger` field beside
`results`; text as a header; `ndjson` as its first line; the four profile
formats have nowhere to put it, and `recording.json` names it.

### What is refused, before anything is launched

Every bound is required, none defaulted, each refused in its own words:
`retain` (without it the run keeps everything), `documents` (without it a
trigger that keeps firing writes forever), `duration` (its end would be a
trigger that may never fire; a launched program's exit is not a bound, since
nothing says the program exits). Also refused: a `trigger` missing, or reading
a field the request does not emit through the backend the resolver chose
(`value`, `kind` and the source's `filter_fields` are what it may read -- any
other is false on every event, so the run would end at its longest looking
exactly like a workload that never did the thing); `on` naming no request or
several; `retain` or `after` not shorter than the run's longest; a request in
`count` or `snapshot` mode (a single reading has no series to retain); a
`window` on any request (a document is a window already); an `escalate:` block
(it attaches layers over the run, and a document cut from it by time would
hold a layer for part of its window); a decomposition (its whole is the
program's life, which a document is one window of). The refusals are in
`Session::check_recording` and `check_trigger_fields` (`src/request.rs`), run
where a session is loaded, where a command line is built, after `--duration`
overrides a file, and after resolution for the fields; `collect::Run::recorder`
refuses the same shapes for a caller that built its requests itself.

## The mechanism, for `DECISIONS.md`

| Decision | Choice | Reason |
|---|---|---|
| The surface | **A session's `record:` block** (`--until` on the command line), beside `duration:`, not `duration: until <predicate>`. | The session is the record of what was asked, and a recording has four more things to say than a duration can hold. `duration` stays what it was -- the longest the run lasts -- which is the bound a recording needs anyway. |
| The bounds | **`retain`, `documents` and `duration` are required, refused when absent; `after` defaults to zero.** | A run with no fixed end is bounded only by what it is told. `after` is not a bound: zero is "the document ends at the round that fired". Nothing is defaulted, so the resolved file has nothing to confess. |
| The predicate | **The filter grammar, over the events one round of the `on` request read, after the request's own filter**, with `value` naming the collector's measured attribute. Fields: `value`, `kind`, the backend's `filter_fields`. | The vocabulary that exists; `value` is rewritten to the collector's attribute in the runner (`read_value_as`), so `src/filter.rs` needed nothing. |
| When it fires | **On the round its predicate becomes true**: a round with a match after one without. A match while a document is open is counted in it (`fired_again`) and writes nothing. | A level held across many rounds is one event of interest. Re-arming on any non-matching round, empty ones included, treats a polled level and a stream of discrete events alike. |
| The window | **`[fire - retain, cut]`, `cut` the first round at or after `fire + after`**, never before the run's open. Windows of two documents may overlap; each is what the retention held. | "The span the retention held when it fired", plus the after-span a burst needs to be in the document whole. The backward reach is exact -- `fired_ns - opened_ns == retain` is asserted. |
| Eviction | **By event timestamp, in the runner's buffers, at most every `retain/16`** (a document is cut from the exact window whatever this is); counted per request, in events and rounds, **only for what no document held**. | A count nobody is shown is a silent loss. An event evicted after a document held it is not a loss. |
| Eviction as a loss | **`Losses::evicted` and `Shortfall::evicted`**, on each document's loss line and beside its distributions -- **without the `>=` mark** (`Shortfall::bounds_below`). | What was evicted is older than the window; every event inside the window is there, so the window's numbers are not lower bounds. They are the window's and not the run's, and the shortfall sentence says exactly that. |
| A document's losses | **The difference since the previous document's cut** (`Losses::since`), plus what was evicted since it. | A ring's drops cannot be placed in time; the delta since the last document is an upper bound for this one's window and is never counted twice. |
| The budget | **Weighed over a rolling span, the longer of `retain` and 2 s**, from the latest budget check at or before its start (`Spending::baseline`). | A share of a run that idled an hour is a share of that hour: the burst after it would be under any ceiling. Measured below. |
| Delivery | **A sink the runner calls as each document is cut**; `main` encodes and writes it at once. | A flight recorder's documents are wanted precisely when the run does not end cleanly. |
| What is not allowed beside it | escalation, windows, decompositions, `count` and `snapshot` modes. | Each would make a document's span disagree with a part of it. |

## Ground truth, measured

`tests/recorder.rs`. The workload is this test binary relaunched with a
variable: it idles 2.5 s, stamps `CLOCK_MONOTONIC`, opens 300 descriptors on
`/dev/null`, stamps, holds them 400 ms, stamps, closes them, stamps, idles
2.5 s and does it again. `proc.fd` walks `/proc/<pid>/fd` every 50 ms, which is
exact. `retain: 1s`, `after: 600ms`, `documents: 5`, trigger `value >= 150`.

- **Exactly two documents** -- the bound of five is not what stopped it; the
  program's exit is -- and no `trigger-3.json`.
- **Each window holds its burst**, against the workload's stamps: the window
  opens before the first open and closes after the last close. Measured:
  document 1 fired 2.6 ms after the burst's last open, its window opening
  951.5 ms before the burst and closing 202.3 ms after it; document 2, 1.2 ms,
  998.3 ms and 200.9 ms.
- **The retention bound, exactly**: `fired_ns - opened_ns` is 1 000 000 000 ns
  in both, and every event of each document is inside its window.
- **To the descriptor**: every reading before the workload's first open is the
  idle baseline, constant; every reading between its last open and its first
  close is the baseline plus 300; the value that fired is that; the two idles
  agree.
- **The evictions, against what the workload says it did before them**: one
  event a round, events equal to rounds, and between 1 and the idle span before
  the window divided by the 50 ms tick (plus two) -- 52 rounds before the first
  window, 40 between the first document's cut and the second window, in the
  run recorded in `recording.json` above. The document's own loss line carries
  the same count and says why.

The accounting is also checked exactly, in `src/collect/mod.rs`, against a
scripted collector that counts its own reads: every read is in a document,
evicted and counted, or reported as unrecorded at the end -- the sum is equal,
not close (`two_held_bursts_write_two_documents_and_every_read_is_accounted_for`).

**A trigger that never fires** (`value >= 100000`, `duration: 2s`, a program
that would run 8 s) ends at its two seconds with zero documents, says "the
trigger never fired" on stderr and `stopped_because: the requested duration
elapsed` in `recording.json`, and counts what it read over the whole run as
unrecorded.

**The budget over a run with no end.** A second workload holds 300
descriptors, spins three seconds of its own cpu time (charged, not wall), stamps
the end, and then spends a tenth of a millisecond every hundred.
`proc.fd` walks its descriptors fifty times a second under `budget: 20%`. The
ordinary run of that session holds: over the whole window the walk is about
3 % of what the program burned. The recording of the same session does not:
it ended for its budget **1986 ms after the workload's spin ended** -- inside
the 2 s rolling span, never during the spin -- with `` `proc.fd` spent
42.36 % of the target's cpu time over the last 2.00s, the span a run with no
fixed end is weighed over against a ceiling of 20 % ``, and `cost.json` says
`within_budget: false` with `weighed_over_ns`, where the whole-window figure
alone would have said it held.

**The refusals** are checked end to end, each before the launch: the workload
writes its report as its first act, and for every refused recording there is
none.

## Stream E's request: `collect::Unserved`

A held request (`window: on <layer>`) the resolver finds no viable backend for
is set aside in `main.rs` with the resolver's reason (a note on stderr before
the run) rather than refusing the run, and handed to
`collect::Run::new(&resolutions).cheaper(&mut ladder).unserved(&unserved)`, which
passes it to `triage::escalate::Policy::for_run_with`. The policy carries it as
a detailed source of its layer with no index in the run: when the cheap pass
names that layer, it attaches what it can serve of it and says the rest was not
served, in the resolver's words. Any other resolver error for a held request
still refuses -- a misspelling is a request's fault, not a layer this machine
lacks.

Measured by `tests/triage.rs::a_held_request_that_does_not_resolve_is_a_layer_this_machine_lacks_not_a_failed_run`:
a program cpu-bound then lock-bound, `cpu.profile` and `cpu.flow` both held
`on cpu`. `cpu.flow` is Intel PT or CoreSight and this machine is AMD; the run
exits 0, attaches the profile when the cheap pass names `cpu`, and the cheap
pass's notes say `(not served: `flow`: this machine cannot serve it: `cpu.flow`
has no viable backend on this machine ...)`. On a machine that serves
`cpu.flow` the test skips, saying why.

## Also landed

- **`emit::stack` counts an event without its weight, rather than refusing.**
  The coordinator traced the `scenario_mem` "mem.latency sample stack" flap
  (stream K's filing) to `stack` reading the weight through `value_of`: an IBS
  op the cpu did not time has a stack and no `latency_cycles`, so about one run
  in ten exited 1. It now reads through `measured`, as every other reducing
  emit does: the event is left out of every stack's events and total, counted,
  and the count is a line in the results' degradations. An attribute that is
  there and not a number is still a refusal. Unit-tested in `src/emit.rs`; the
  scenario itself is stream D's file this wave. This is the coordinator's
  forwarded request (a), `docs/flaky.md` #20: fixed, and #20 can be closed
  once the scenario has run green on main for a while.
- **`omn list --format json` carries the sweep** (stream M's filing): each
  entry gains `here` (`resolve::availability`, the text table's `HERE`) and
  `probes`, per backend in chain order its `collector` and every requirement
  with what its probe answered. It needed no change in `src/resolve.rs`: the
  probes are `Capabilities::probe`, which is public, over each backend's
  `requires`. `tests/runner.rs::the_json_listing_carries_the_sweep_the_text_table_shows`
  holds the JSON's tallies to the text footer. `mcp/src/binary.rs::parse_sweep`
  can go.
- **`--duration` over a recording** (the coordinator's forwarded request
  (b): stream M's `omn-mcp` passes `--duration` to `run_session`):
  `--duration` on `-f` replaces the recording's longest duration, as it
  replaces every request's, and the recording's bounds are
  checked again against it -- a `retain` the new duration no longer exceeds is
  refused, exit 2, before anything runs. So a session "until a predicate fires"
  given `--duration 10s` records for at most ten seconds, still ending earlier
  at its `documents` or the program's exit.
- **The session's text dump shows the `record:` block** (`render::session`).

## What it does not do, and why

- **A document's cost is the run's so far**, not its window's: `Spending` has
  no snapshot at the window's open. The rolling budget is what holds a
  recording to its ceiling; `cost.json` at the end is the whole run's.
- **Stacks are read when a document is cut**, through `Collector::stacks`,
  which is documented as read after `stop`. For `cpu.profile` and
  `sched.offcpu` the table is current at any moment; a collector that
  symbolises only at `stop` would give a document ids without frames. None
  does today; a test would be needed before one did.
- **What a collector keeps in itself is not evicted**: a profile's stack table,
  a symboliser's maps. The retention bounds the events; a stack table grows
  with the distinct stacks a run sees, which is bounded by the program, not by
  the run's length. Not measured over hours.
- **Two-phase drains** (`Rows::retire` / `take`, D's half) were not used: D's
  half did not land on this branch's base, and the brief allows either.
- **A join in a document** is applied to the window's events as to any run's.
  A state opened before the window and closed inside it is not in the
  document; the join says so as it says it of any run's start.

## The escalation shedding test

The coordinator noted `tests/triage.rs::a_detailed_layer_over_its_budget_is_let_go_and_the_escalation_goes_on`
is load-sensitive. This stream's changes do not touch the shed path: a
non-recording run's budget is weighed exactly as before (`Spending::baseline`
returns the collection's own start when no rolling span is set, which a unit
test holds), so it is left as the flaky tally has it.

## The check before the commit

`cargo test --test recorder` (8) and `cargo test --test runner` (22) green.
One `make check`: red on `perf::tests::closing_a_group_gives_its_descriptors_back`
(64 groups left 92 perf descriptors behind) -- not in the tally, not this
stream's file, and it counts the process's perf descriptors while the other
lib tests open theirs in parallel; it passed in the run below. Then one
`cargo test --workspace --no-fail-fast`, four failures:

- `tests/cli.rs::the_session_indexes_hold_exactly_the_sessions_on_disk` --
  expected, the two index rows below.
- `collect::tests::weighing_a_run_costs_a_fraction_of_the_run` -- tally #12
  (13.0 ms of a 604 ms window); passed alone. It is this stream's by the
  tally, and left: the fix the tally names (time the weighings on
  `CLOCK_THREAD_CPUTIME_ID`) changes what `cost.json` reports as the
  weighings' cost, which wants its own commit and its own budget runs.
- `family_sched::a_task_moved_between_two_cpus_is_counted_once_per_move` --
  tally #5; passed alone.
- `scenario_sched::a_workload_that_is_half_blocked_measures_as_half_blocked`
  -- tally #2; passed alone.

## Tests

- `tests/recorder.rs` (new): the two-burst ground truth, a trigger that never
  fires, the refusals before the launch, structured output on stdout refused,
  the command line, the rolling budget.
- `src/collect/mod.rs`: the accounting against a scripted collector, the bound
  on documents, a trigger that never fires, `value` rewritten, the rolling
  baseline.
- `src/session.rs`: a recording's round trip through YAML, JSON and TOML, every
  missing bound, a malformed trigger, a field the source does not emit.
- `src/triage/escalate.rs` and `tests/triage.rs`: a named layer this machine
  cannot serve, unit and end to end.
- `src/emit.rs`: a stacked event without its weight.
- `tests/runner.rs`: the JSON sweep against the text table.

## Index rows

For `examples/README.md` and `GUIDE.md` -- the session-index test in
`tests/cli.rs` is red on this branch until they are added:

```
| `record-descriptor-bursts.yml` | Can a run wait for something to happen and keep only what led up to it? A program that opens 300 descriptors twice gets exactly two documents, each holding one burst and the second before it. |
| `record-until-it-never-fires.yml` | What does a recording whose trigger never fires leave behind? No document, the run ended at its longest, and what it read counted as in none. |
```

## For `DECISIONS.md`

The table under "The mechanism" above, as "Settled in the twelfth wave: the
flight recorder". In one paragraph: a recording is a mode of a session -- a
`record:` block with a trigger in the filter grammar over one request's rounds,
and three required bounds (`retain`, `documents`, `duration`); it fires on the
round its predicate becomes true, cuts `[fire - retain, fire + after]` into a
document written at once, evicts by timestamp and counts what no document held
as `Losses::evicted` (a loss, surfaced, not a lower bound on the window), and
weighs its budget over a rolling span of `max(retain, 2 s)`. And, for stream
E's request: a held request that does not resolve is a layer this machine
lacks, set aside and reported by the escalation, not a failed run.

## Filed for another stream

Nothing blocking. `src/filter.rs` needed no addition (`value` is rewritten in
the runner), and `resolve::probe_all` did not need to be made public for the
JSON sweep.
