# The join engine and the decomposition

Six waves of the engine's stream are recorded here, the newest first. The
sixteenth wave bounded the engine's memory, compared an interface by its index
and let an `overlap` rule say which attribute of a drained row is a window. The
fifteenth wave held the engine to its phase 1, 4 and 5 exit criteria: a
signed integer that survives a round trip, a row that cannot go without its
evidence level, a load test, an eviction seen end to end, and a boundary
added by a session alone. The fourteenth
wave ran the use case `omn-mcp` was built for end to end and paid the
engine's last filed edge, a join with no key. The thirteenth
wave gave the engine the words and the match kinds the crossings asked for,
and stacks in their source's names. The eleventh wave paid four debts the
tenth wave found in the engine. Each was a place where the engine said more,
or less, than it had measured. The eighth wave built the engine's third rung
(`TEMPORAL`), the decomposition and the chains. Their records follow
unchanged.

---

# The sixteenth wave — the engine's memory, and the keys it compares

| | |
|---|---|
| Delivered | an `iface` key compared by the kernel's index; `KeyValue::proc_of`; `overlap`'s `enclosing`; the engine's attributions bounded to the rule's `top` as states end, and a stream already in order walked where it lies; the memory decomposition's test held to a ready interpreter |
| Refused, new | `enclosing` on any match but `overlap` (`JoinError::EnclosingOutsideOverlap`); a drained row without the attribute `enclosing` names (`MissingSpan`, as for `span`) |
| Tests | `src/event.rs` 2 new; `src/join.rs` 3 new; `tests/join_engine.rs` 1 new, 2 widened; `tests/cross_machine.rs` 1 widened |
| Sessions | `examples/join-iface-loopback.yml` |
| Files | `src/join.rs`, `src/event.rs`, `tests/{join_engine,cross_machine}.rs`, `examples/{join-decompose-memory,join-iface-loopback}.yml`, `docs/keys.md`, this document |
| Machine | 24 cpus, kernel 7.2.7, five other streams, `/home` full for most of the wave (`docs/fixes/w16-engine.md`) |

## 1. An interface is its index

`join::identity` compared `iface` by its name. `net.iface` writes one series
per counter kind (`lo/drops`), so it never met `net.bytes`'s `lo`, and a
rename made a second key. `KeyValue::iface` now fills an index of 0 from the
name at the read (`if_nametoindex` on the part before a `/`, which no Linux
interface name holds), and the identity is `iface#<index>`, the name only when
the index stayed 0. `between` keeps each reading's baseline on the full key, so
two series of one interface never difference each other. Held by
`join_engine::two_sources_of_one_interface_join_by_it_and_credit_what_lo_moved`:
`net.iface` and `net.bytes` of `lo` joined by `between`, the bytes credited at
least what `/sys/class/net/lo/statistics` counted across the test's own 2000
datagrams and at most what it counted across the run. Before: 0 of 20 readings
matched, nothing credited. After: 20 of 20, 4 112 000 bytes for a burst the
kernel counted at 4 112 000.

## 2. `proc` has a maker

Kept, not removed: `CATALOG.md`'s container trap names it as the answer to a
pid being ambiguous across namespaces. `KeyValue::proc_of(pid)` reads the pid
namespace's inode from `/proc/<pid>/ns/pid` and the pid inside it from the last
`NSpid`, and is tested against the namespace link's own text. No collector
calls it yet; the per-process sources are filed (`docs/open-findings.md`, the
sixteenth wave, stream engine).

## 3. `overlap`'s `enclosing`

`DECISIONS.md` left open how a drained row of several episodes is credited. The
rule now says it: `enclosing: <attr>` names the attribute holding the length of
a window, from the row's stamp, that encloses every occurrence. A row whose
window lies in one interval is credited whole (exactly: every occurrence is
inside); one that crosses an interval's end stays uncredited. Absent, `span` is
a sum and the row is uncredited, as before.

On the reclaim crossing, against the cgroup's own `memory.stat` (pgsteal):

| enclosure carried by `mem.reclaim` | credited in the program's intervals |
|---|---|
| none (today) | 745 of 41 413 pages |
| the drain's close less the first episode's start | 165 of 36 937, 143 of 42 810 |
| the last episode's end less the first's start | 35 225 of 37 657, 40 497 of 40 739, 41 909 of 42 093 |

The drain and the pressure poll are both 100 ms and out of phase, so a window
closed by the drain almost always crosses a reading. The last episode's end
needs eight bytes in the row and one store in `src/bpf/mem_stall.rs`; it was
measured with that patch applied locally and reverted (it is the mem and
coordinator layer's). Filed with the patch; the session line
`enclosing: enclosing_ns` lands with it, and
`cross_machine::a_memory_limit_turns_...` then holds half the cgroup's pgsteal
credited at least.

## 4. Memory that does not grow with the run

The engine kept a record of every state that ended until it ranked them, and
sorted an index of references to the whole input. Both were linear in the run.
`Ranked` now folds (`wakeup`, `woke`) or ranks and cuts back to `top` (every
other match) as states end, which is exact: the cut is the final stable sort.
And `apply_within` walks a stream already in the engine's order where it lies.
Measured by `join_engine::a_join_under_load_...` on the 1.5 M events
(500 k then 1 M), beyond the stream:

| | 500 k | 1 M |
|---|---|---|
| before | 48.7 MiB | 97.2 MiB |
| attributions bounded | — | 15.4 MiB |
| and an in-order stream walked in place | 0.7 MiB | 0.7 MiB |

The test now holds every peak under 4 MiB and the long stream to the short's
peak plus 1 MiB. What is still linear is not the engine's: `main.rs` clones
every collection's events into one unsorted `merged` vector, so the engine
falls back to its index (12 bytes an event) there. Filed. A rule still cannot
choose the kernel or the query tier: the kernel tier is a rule compiled into a
program of `src/bpf/` per match kind, and the query tier a join over stored
runs, which `omn` has no command for. Cost: phase 5's "adding a boundary means
adding a rule" stays `[~]`, for the agent tier only.

## 5. The hour

`cargo test --release --test join_engine -- --ignored an_hour_of_joins` runs as
written; `OMN_JOIN_LOAD_SECS` shortens it. Every round asserts the state
balance, every count against the generator, the peak against the first round
and the 4 MiB bound, and nothing live after.

---

# The fifteenth wave — the engine held to its exit criteria

| | |
|---|---|
| Delivered | `join::decompose_within` (R's filed request); `AttrValue`'s canonical integer and its equality; `Breakdown` refusing a row without an evidence level when read; the open states' age index and the unmatched attributions no longer kept; the load test, the hour test, the end-to-end eviction and the new-boundary test; skip lines that carry their reason |
| Refused, new | a decomposition table read back (a `results.json`) whose row carries no evidence level, or whose whole was read from a source and carries none |
| Tests | `src/event.rs` 1 new, 1 widened; `src/join.rs` 2 new; `tests/join_engine.rs` 4 new (one `#[ignore]`); `tests/vertical.rs` 1 new; `tests/cross_memory.rs` 1 new |
| Sessions | none added: the two sessions of the end-to-end tests are written by the tests |
| Files | `src/join.rs`, `src/event.rs`, `tests/{join_engine,vertical,cross_memory,cross_machine,cross_compute}.rs`, `mcp/tests/{protocol.rs,common/mod.rs}`, this document |
| Machine | 24 cpus, kernel 7.2.6, `CAP_BPF` and `CAP_PERFMON` on the binary, loaded by five other streams and a matrix sweep |

## 1. A paired part pairs over the joins' windows

`main.rs` gives `join::apply_within` every source that ran to the end with
`closed_ns = u64::MAX`, because what a stop drains is stamped after the
window's close; `decompose_with` rebuilt the windows from the results and cut
the pairs at the real close. `decompose_within(.., windows)` takes the map the
joins were given, and `decompose_with` is now `decompose_within(..,
&windows_of(results))`. The results' windows still decide the window row and
whether an absent key is a zero. The swap of the two call sites is the
resolver stream's (`docs/open-findings.md`, fifteenth wave).

## 2. A signed integer reads back as what was written

`AttrValue` is untagged on the wire, and `5` does not say whether it was an
`Int` or a `Uint`: the reader answered `Uint`, so an `Int(0)` (an `fsync`'s
`ret`, a process's `exit_code`) came back as another variant. The fix keeps
the wire as it is, since every consumer reads a number as a number: an
integer has one canonical form, `Uint` when non-negative and `Int` only when
negative, which `Attr::int` and the new `AttrValue::int` produce, and
`AttrValue`'s equality holds `Int(n) == Uint(n)` for `n >= 0`, so an `Int`
built by hand is still the integer it holds. The round-trip test covers every
`KeyValue` and `AttrValue` variant, a non-zero `ts_err_ns`, and zero, negative,
`i64::MIN`, `i64::MAX` and `u64::MAX` values, and compares the `Debug` form as
well as equality, so the variant itself is held.

## 3. No row without its evidence level

Every row the engine builds is rated (`Readout::row`), and only a whole that
is a bracket (`Kind::Window`, `Kind::Lifetime`, `Kind::is_bracket`) goes
without a level, since nothing joins it to the key. `Breakdown` now
deserializes through a check: a row with no level, or a whole read from a
source with none, is refused with the row and the key named. The vertical
case's `block-io` table is held to `IDENTITY`, row and whole, and its device
row to the server's own count of `O_DIRECT` pages.

## 4. Under load

`join_engine::a_join_under_load_ends_every_state_it_opened_and_holds_no_more_than_its_map`
builds 1.5 million block-request events over 1200 recycled pointers, through
a `scoped_eq` rule with a 10 ms window and a map of 1000, in segments in each
of which one thing can happen to a state: done, reissued before done (the
stale segment unmatched), expired, evicted from a full map, keyless, and in
flight when the `from` window closes. Every count is the generator's own,
written as it built the stream, and the balance holds against it. The memory
is counted by a counting allocator in a child process of the test binary:
the peak is linear in the stream (a twice longer stream takes 2.0x), and
nothing is left live after the engine returns.

What it found, fixed here:

- **Expiry walked every open state on every event**, and eviction on every
  insert: a pass over the map per event, which is quadratic in a full map.
  The states now keep an age index (`States::ages`), and both walk only the
  oldest; states still end in key order, so no output moved.
- **Every state that ever ended was kept until the end**, to be dropped at
  ranking if it matched nothing. Only `wakeup`'s chains read those (they count
  every wakeup), so every other match drops them when they end. Measured at a
  million events: 179 MiB of peak beyond the stream, down to 97 MiB.

The hour, by hand (`an_hour_of_joins_leaks_no_state`, `OMN_JOIN_LOAD_SECS`
to shorten it): rounds of two million events, each held to its own counts, the
peak of each to under twice the first's and the heap after each to the
baseline.

## 5. An eviction, end to end

`a_join_that_evicts_says_how_many_in_results_json_and_in_the_text` runs
`power.freq` to `power.cstate`, by cpu, with a map of two and a window of
5 ms, polled every 100 ms. From the kernel's count of online cpus and the
rule alone: nothing closes (each poll reads every cpu in turn), the map peaks
at two, the overflow and the expiry are both non-zero, and the states balance.
The counts are `results.json`'s, the text rendered from that document prints
them on its `EVICTED:` line, and a text-format run's own lines balance.

## 6. A boundary is a session's rule

`a_new_boundary_is_a_rule_in_a_session_and_no_line_of_engine`: no catalog id
is a string in `src/join.rs` outside its tests, and the same two sources,
which no test or session had joined, run through a join the session writes;
each cpu is credited exactly its `power.cstate` readings from its first
frequency reading on, counted off the raw events. The kernel and query tiers
are still not a rule's choice, so the criterion stays partial.

## 7. Skip lines

`cross_memory`'s, `cross_compute`'s and `omn-mcp`'s skips put the machine's
reason on the skip line and indent the rest (P's gate saw a skip with nothing
after it). `cross_machine`'s churn says what the program printed, or that it
printed nothing, and how it exited. The three `mcp/tests/protocol.rs` tests D
filed already skip with the resolver's reason since `8e4d7d2`.

---

# The fourteenth wave — an agent benchmarks a program

| | |
|---|---|
| Delivered | the `omn-mcp` walk-through, end to end (`docs/mcp.md`'s first section, `examples/mcp/`); `Rule::via: Option<KeyKind>`, `overlap` with no key, `JoinError::NoKey` and `JoinError::SeveralSeries`, `join::MACHINE`; `omn-mcp`'s band words for `extreme`; the CI's paranoid-2 and paranoid-4 failures in J's files skipping with the machine's reason |
| Found landed | `Cost::worst_case_percent` (twelfth-wave request to V), now read by `omn-mcp` instead of its copy |
| Refused, new | a rule with no `via` on every match but `overlap` (`NoKey`, before anything runs); a keyless `overlap` whose `from` source carries several series (`SeveralSeries`, by name, with up to three of them) |
| Tests | 5 unit tests in `src/join.rs`; `tests/join_engine.rs` +2 (the keyless scenario, the validation refusal) and the task-keyed overlap scenario shared with it; `mcp/tests/walkthrough.rs` (new, 2); `mcp/tests/protocol.rs` 4 changed for the stock machine; `tests/cross_compute.rs` (4 through its runner) and `tests/cross_machine.rs` (1) likewise |
| Sessions | new `examples/join-overlap-machine.yml`; new `examples/mcp/approved/{benchmark,profile,decompose,triage}.yml` and `program.py`, and the client configuration `examples/mcp/mcp.json` |
| Files | `src/join.rs`, `mcp/src/session.rs`, `mcp/tests/{protocol,walkthrough,common/mod}.rs`, `tests/{join_engine,cross_compute,cross_machine}.rs`, `docs/mcp.md`, the sessions above; outside, declared below: `src/validate.rs` (`check_joins`, +6/-2) |
| Machine | 24 cpus, kernel 7.2.6, `CAP_BPF` and `CAP_PERFMON` on the binary, `perf_event_paranoid` -1, loaded by five other streams |

## 1. The use case, end to end

`docs/mcp.md`, "The walk-through", is the deliverable: the person approves a
directory of sessions (`examples/mcp/approved/`: a launched program's
benchmark, a profile, a decomposition, the triage's cheap pass), starts the
server with `--allow-run 20s --allow-launch` on it, and the agent calls
`list_approved`, `run_session` and `read_run`. The walk-through says, field by
field, where in `run_session`'s answer a model finds what it needs to compare
two runs: the benchmark's number (`result.lifetime`), the shortfall
(`what_it_is_missing`), the evidence level (`evidence_means`), and the spread,
which is the model's to compute from N answers with the bench's statistics --
the formula and the gate's two conditions are written there.

The program (`program.py`) computes, sleeps 200 ms and computes again, and
says what it did on its own clock and from its own `schedstat` on stderr,
which reaches the agent in `said_by_omn` because the launched program inherits
`omn`'s stderr. Measured by the test, one run each:

```
decompose.yml:  life 741.8 ms, the program's own span 726.3 ms, asleep 200.1 ms, on a cpu 524.3 ms
triage.yml:     the escalation, for latency, named `cpu` at 0.41 s, attaching `profile`
```

The decomposition's biggest row is `on-cpu`, within one 50 ms poll of the
program's own account, and `blocked` holds the sleep.

## 2. Measured, not described

`mcp/tests/walkthrough.rs`, against a copy of the approved directory:

- **every number and loss handed over is in the files.** Each served session's
  `run_session` read-back and its `read_run` (every event) equal the run's
  `results.json`, `resolved.json` and `cost.json` key by key, less the two
  documented additions; every non-zero loss is restated first and no zero one
  is. And the numbers are the program's: each life holds the program's own
  span; `blocked` holds its sleep; the dominant row is the one the program's
  figures name.
- **the spread.** Five `run_session` calls of `benchmark.yml`; the bench's own
  `Stats` (`examples/bench/harness.rs`, included by path) from the answers
  equal those from the files to the bit:

  ```
    WORKLOAD      N   MEDIAN        IQR           CV       RESOLVES   GATE
    handed         5   579.56ms      196.39ms       27.42 %   43.47 %   REFUSE
    files          5   579.56ms      196.39ms       27.42 %   43.47 %   REFUSE
    own clock      5   568.92ms      181.34ms       27.74 %   43.97 %   REFUSE
  ```

  At five runs on a loaded machine the gate refuses, which is what the
  walk-through tells an agent to say.

Not rehearsed beside a deliberate load: spinning 24 shells beside the test was
refused in this session by the environment's permissions. The runs above were
made beside five other streams' suites.

## 3. `overlap` with no key (`open-requests 5032`, the thirteenth wave's filing)

```yaml
- {id: waiting-per-pressure-interval, from: mem.pressure, to: sched.futex,
   match: overlap, value: total_ns, span: total_ns, stamped: start}
```

`via` absent: every `from` reading is one series, keyed `machine`
(`join::MACHINE`), and every `to` event -- whatever it is keyed by -- is laid
across its intervals as a keyed `overlap` lays it across one key's. Nothing
is counted `keyless`, since no event can lack a key nobody asked for. Two
refusals keep the absence from meaning anything else:

- **a `from` source that carries several series** (a poll of `io.bytes` or
  `io.inflight` per device, of `power.freq` per CPU) is refused by name with
  up to three of them: each series' reading would close the other's interval,
  and the "interval" would be the gap between two devices. Found at apply
  time, since a key set is known only from the events. Through the binary,
  `with_no_key_a_per_cpu_poll_is_refused_with_as_many_series_as_the_machine_has_cpus`
  holds the count in the refusal to the kernel's own
  `/sys/devices/system/cpu/online` (24 here: "carried 24 of them (cpu:0,
  cpu:1, cpu:10)"). (`proc.threads` is not an example: it is one reading per
  process, so a launched program is one series.)
- **every other match kind** refuses a rule with no key (`NoKey`, from
  `join::check`, so before the run, and from `omn --dump-resolved`).

Ground truth, `a_wait_is_split_across_the_machines_readings_with_no_key`: the
thirteenth wave's four 250 ms waits, laid across the machine's memory
pressure polled every 100 ms (`examples/join-overlap-machine.yml`); each
interval is the reading's own `interval_ns`, and each credit is held to the
overlap of the program's own brackets with it, with the same slack the keyed
scenario carries. Measured by one run of the session here: ten intervals credited, 100.02 to 99.86 ms for
the eight inside a wait, 60.1 and 40.4 ms at the two edges -- 1000.23 ms in
all against four brackets of 250 ms. The separation: the same rule keyed by
`task` fails on the machine's readings (`MissingKey`), unit-tested.

The crossing that asked, `examples/cross-machine-reclaim-io.yml`, now carries
the keyless rule itself: `reclaim-per-pressure-interval`, from the machine's
`mem.pressure` to `mem.reclaim`, `value: pages_reclaimed`, `span: total_ns`,
`stamped: start` (an event of one episode begins at its stamp and lasts its
`total_ns`). `tests/cross_machine.rs` runs it in a delegated memory cgroup
capped at 32 MiB (this machine delegates one under `user@1000.service`; the
earlier line here that it did not was wrong) and holds it to the cgroup's own
`memory.stat`: every page the cgroup reclaimed while the program ran is either
credited to a pressure interval the program's own brackets overlap or counted
uncredited; credited plus uncredited equals every page the tool saw reclaimed,
within one; and on a machine where every reclaimed page is the cgroup's, no
credited page lies outside the program's brackets. Measured, three runs:
38 519 to 41 644 pages reclaimed, 272 to 455 credited, all inside the
brackets once the span is the episode's own (with `span: window_ns` 104 of
376 fell after the program ended -- the drain window encloses an episode, it
is not one), and 41 184 to 41 864 uncredited.

**What that measures about the engine**: almost all of it is uncredited, by
design. `mem.reclaim` drains a histogram, and a row of several episodes has no
one window to place (`several()`), so its amount is counted rather than
spread. The same holds for every drained source. A row of several episodes
whose enclosing window -- `[ts, ts + window_ns]` for `mem.reclaim`, stamped at
its first episode -- fits inside one interval could be credited whole, and
exactly; the engine cannot tell an enclosing window from a sum of durations
by the attribute's name, so that needs the rule to say which (open, below).

## 4. The band for `extreme` (R's thirteenth-wave request)

`omn-mcp`'s `band()` now says "over 20 %, with no bound above", `main.rs`'s
words, and "about zero, under 0.1 %" for `free`, and reads the ceiling from
`Cost::worst_case_percent` instead of holding a second copy. The catalog's doc
comment is P's, filed.

## 5. The stock machine in J's files (the CI's two runs)

At `perf_event_paranoid` 4 (run 1) and 2 (run 2) the kernel refuses a
sampling `perf_event_open` with a callchain, so `cpu.profile` has no viable
backend, and every session that profiles is refused with exit 1. The tests
now say so instead of failing:

- `tests/cross_compute.rs`: the runner skips when `omn` exits 1 with *no
  viable backend on this machine* (or *cannot start collecting* beside
  `perf_event_paranoid`), printing the machine's lines and what would have
  been checked. Four tests in both runs.
- `tests/cross_machine.rs`, `a_guardrail_this_machine_cannot_serve_refuses_the_whole_crossing`:
  at paranoid 4 the count (`cpu.instructions`) is refused before the
  guardrail, so the refusal names the count. The test asks the count alone
  with `--dump-resolved`; if the machine refuses it too, the crossing cannot
  be created and it skips with that reason; if the count is served, the
  refusal not naming the guardrail still fails.
- `mcp/tests/protocol.rs`: three tests skip on the same condition; the plan
  test checks the plan's refusal against the binary's line for line instead
  of parsing an empty `--dump-resolved`.

Checked against the CI logs' text (exit status 256, the messages quoted in
`../omniscient-ci-logs/`); the next push's CI is the check on a stock kernel.

## Declared: the one edit outside stream J's paths

| File | Edit | Why |
|---|---|---|
| `src/validate.rs` | `check_joins`: `let Some(via) = rule.via else { continue };` before the key check, and the rejection names `via` for `rule.via` (+6/-2, one line a comment; the message is unchanged) | `Rule::via` became an `Option`, and the validator does not compile without it. It is exactly the validator half filed for P in the thirteenth wave (skip the key check with no key); the other half, refusing an absent `via` elsewhere, is the engine's `NoKey`. Filed for P so the lines are kept if P reworks them. |

## Handoff

- **Done, with scenarios**: the walk-through (items 1 and 2), `overlap` with no
  key (item 3), the `extreme` band (item 3), the paranoid-4 failures (item 4).
- **The index rows** (`examples/README.md`, `GUIDE.md`):

  | Session | What it shows |
  |---|---|
  | `join-overlap-machine.yml` | A thread's waits laid across the machine's memory-pressure readings with no key: `overlap` over state the machine holds once. |

  `examples/mcp/approved/*.yml` are not read by the index test (it reads
  `examples/*.yml`); `docs/mcp.md`'s walk-through is their index. A row for
  `GUIDE.md` pointing at it, if wanted: *"`docs/mcp.md` -- an agent
  benchmarks a program through `omn-mcp`, and says what dominates its time."*
- **For `CATALOG.md`** (the join notation): `via` is optional for `overlap`
  only, and an `overlap` without it reads its `from` source as one series,
  keyed `machine`.
- **For `DECISIONS.md`**:
  - *A join with no key is `overlap`'s alone, and reads one series.* A
    keyless rule of any other kind is refused before the run; a keyless
    `overlap` over a source with several series is refused by name at apply
    time rather than read as one. Chosen over a `machine` key variant because
    `KeyKind` names what an event carries, and machine-wide state carries
    nothing.
  - *The spread of a repeated benchmark is computed by the model, from N
    answers, and the walk-through says how* (reaffirms the thirteenth wave's
    no-tool decision, with the use case run).
  - *The approved sessions live in `examples/mcp/approved/`, the client
    configuration beside the directory*, since `list_approved` lists every
    session-shaped file under it, `.json` included.
- **Filed**: `docs/open-findings.md`, "the fourteenth wave, stream J": P
  (`src/validate.rs::check_joins`, the declared lines to keep; `src/catalog.rs`,
  `Cost::Extreme`'s doc comment).
- **After the adversarial review** (one commit):
  - the walk-through's floor is per read (20 numbers; `benchmark.yml` writes
    41), not 500 over the walk-through, which a benchmark-only machine (the
    paranoid 2 and 4 cells) could never reach;
  - the validator edit is `check_joins`, +6/-2 with the rejection's format
    string unchanged, and the filing for P names that function (it said
    `check_sources`, which does not exist);
  - the keyless-refusal scenario's `overlap` half now requires the session to
    resolve (it was passing on an unrelated refusal: `sched.futex` has no
    `system` target), and `SeveralSeries` has a scenario through the binary
    held to the kernel's CPU list;
  - item 3's ground truth is now the crossing that asked, the reclaim
    crossing, against the cgroup's own `memory.stat` (above);
  - the guardrail test skips only on the machine's refusal of the count
    (exit 1, naming `cpu.instructions`, "no viable backend on this machine"
    or `perf_event_paranoid`), and fails on any other probe error.
- **Open**: an `overlap` rule attribute saying its `span` *encloses* a
  drained row's occurrences rather than sums them (`window_ns` on
  `mem.reclaim`), so a row of several episodes whose window fits in one
  interval is credited whole. Measured: 41 184 of 41 639 reclaimed pages are
  uncredited today on the reclaim crossing, counted and said.
- **Matrix cells this should move** (the coordinator's, at the end of the
  wave; the judges' commands are `scripts/matrix.sh <cell>`):
  - `ci-6.17.0-nocaps` at paranoid 4 (the CI's first run): `cross_compute`
    4 failed -> 4 skipped with the machine's reason; `cross_machine`
    `a_guardrail_...` failed -> skipped; `omn-mcp` `protocol` 4 failed -> 3
    skipped and the plan test passing on its refusal half;
    `walkthrough` new: `benchmark.yml` served, the other three skipped by
    `list_approved`'s reason, and the per-read floor met (41 numbers per
    read, measured with a benchmark-only run). Check: the next push's CI, `cargo test -p
    omn-mcp` and `cargo test --test cross_compute --test cross_machine`.
  - the paranoid-2 cell (run 2): the same, and `decompose.yml` and
    `triage.yml` skipped without `CAP_BPF`.
  - every cell: `join_engine`'s
    `with_no_key_a_per_cpu_poll_is_refused_with_as_many_series_as_the_machine_has_cpus`
    passes where `power.freq` reads every online CPU, skips where it has no
    backend, and fails -- on purpose -- where a backend reads fewer CPUs than
    `/sys/devices/system/cpu/online` lists: that is a finding about the
    source, not the join. Check: `cargo test --test join_engine per_cpu`.
  - `here-caps`, `here-nocaps`: no source's availability changes; on
    `here-nocaps` the walk-through serves `benchmark.yml` and `profile.yml`
    (paranoid -1) and skips the two eBPF sessions.
- **The suite, after the review's fixes**: `make check` red only at the
  session-index test (`join-overlap-machine.yml`, the row above); the
  `cargo test --workspace --no-fail-fast` after it: **2010 passed, 1 failed
  (that index test), 8 ignored**; clippy (workspace, all targets) and `cargo
  fmt --check` clean.
- **The suite, before the review**: on this branch with `main` merged (streams R's and U's
  foundations), with the capabilities, beside five other streams. Own files:
  `cargo test --lib join` (81), `tests/{join_engine, cross_compute,
  cross_machine}.rs` and `-p omn-mcp` (protocol 10, walkthrough 2) pass.
  `cargo clippy --all-targets -- -D warnings` (both packages) and `cargo fmt
  --check` are clean. The last `make check` stopped at the session-index test,
  red for `join-overlap-machine.yml` as expected. The `cargo test --workspace
  --no-fail-fast` after it: **2005 passed, 5 failed, 8 ignored** -- the index
  test, and four each rerun alone once:
  `cross_machine::a_memory_limit_turns_a_cached_read_into_device_reads_and_each_byte_carries_its_rung`
  (J's file, unchanged this wave: the uncapped run found 32 pages of reclaim
  under the cgroup's id where its own count said 0 -- another stream's memory
  pressure; passed alone), `encoders::a_known_allocation_lands_on_the_chrome_trace_at_the_time_it_was_made`
  (in the tally; passed), `runner::a_counted_window_over_a_polled_counter_ends_with_its_closing_read`
  (not in the tally, R's file: no `lo` series; passed alone), and
  `scenario_mem::both_huge_page_backends_read_the_same_promotion_counter`
  (in the tally, K's file: failed again alone, 883 huge pages against
  meminfo's 633 to 635 -- left to K, not rerun further).

---

# The thirteenth wave — the engine says what it knows

| | |
|---|---|
| Delivered | `omn-mcp --allow-launch DIR` and `list_approved` (item 7, added mid-wave); `MatchKind::Woke` and `MatchKind::Overlap`; `Rule::woken`, `Rule::span`, `Rule::stamped`; a decomposition part a join defines (`value: paired`, `join::PAIRED`), with `join::check_decomposition_in` and `join::decompose_with`; `StateStats::scope_collapsed`, and `within_key: TEMPORAL` on a `scoped_eq` that saw one; `stack::demangle` and `Frame::mangled`, so every stack reads in its source's names |
| Found landed, and given its ground truth | the evidence ladder in words (`0dbc3ad`) and the interrupt named as the interrupt (`d32dab8`), both twelfth wave |
| Refused, new | `woke` on a key that is not `task`; `overlap` with no `value`, or from a source collected in `trace`, `sample` or `count` mode (before the run); a `to` event missing the rule's `woken` or `span` attribute (by name); a `paired` part naming no join, a join that is not `woke`, or given `less` or `reading` |
| Tests | 10 unit tests in `src/join.rs`, 1 in `src/stack.rs`; scenarios: `tests/join_engine.rs` +2 and 3 strengthened, `tests/cross_io.rs` +1 and 2 strengthened, `tests/cross_compute.rs` 2 strengthened, `tests/stack_capture.rs` +2, `mcp/tests/protocol.rs` 1 widened to all four levels |
| Sessions | new `examples/join-interrupt-or-task.yml`, `examples/join-overlap-waits.yml`; changed `sched-wakeup.yml` (its join), `cross-io-one-read.yml` (the `woke` join), `join-decompose-memory.yml` and `cross-compute-where-the-time-went.yml` (what the workload reports), and two comments |
| Files | `src/join.rs`, `src/stack.rs`, the `MatchKind` enum of `src/catalog.rs`, `mcp/src/{main,server,session,binary}.rs`, `mcp/tests/protocol.rs`, `docs/mcp.md`, `tests/{join_engine,cross_io,cross_compute,cross_machine,stack_capture}.rs`, the sessions above; outside, declared below: `src/encode.rs` (four lines), `Cargo.toml` and `Cargo.lock` (two crates) |
| Machine | 24 cpus, kernel 7.2.6, `CAP_BPF` and `CAP_PERFMON` on the binary, loaded by four other streams; no test here skipped on it |

## 1. The evidence ladder in words — landed, now checked on every rung

`Evidence::establishes` and `does_not_establish` landed in the twelfth wave
(`0dbc3ad`), and `mcp/src/evidence.rs` already reads them: its own copy is
gone. What was missing was the ground truth the brief asks for, which covered
two rungs. `identity_and_temporal_never_read_alike` now runs one real session
per rung and reads each back through `omn-mcp`:

| Rung | Session | Join |
|---|---|---|
| TEMPORAL | `join-between-residency.yml` | `between` |
| WAKEUP | `sched-wakeup.yml` | `wakeup`, now declared (below) |
| KEY | `cross-memory-which-allocation.yml` | `interval` |
| IDENTITY | `vertical-one-request.yml` | the rows read per `req` |

Every `evidence` the documents carry must be annotated with exactly the
engine's two sentences and its rank, and no two levels may share a rank or a
sentence. Measured: `read back with its sentence: ["IDENTITY", "KEY",
"TEMPORAL", "WAKEUP"]`. A rung the machine cannot produce (no `CAP_BPF`, no
shim, the vertical server not built) is skipped by name and the rest run.

The text renderer still prints the bare word. That is R's file: filed.

## 2. A waker of 0 is not always the idle task — landed, now checked end to end

The chain's rendering landed in the twelfth wave (`d32dab8`), tested on
events built by hand. The ground truth is a real program:
`examples/join-interrupt-or-task.yml` launches a python thread that sleeps
twenty times (each `nanosleep` ended by its hrtimer's hard interrupt) and then
reads a pipe twenty times, each read released by a helper thread's write after
the helper's own sleep. It prints both thread ids. The join, two hops deep:

```
  heaviest wakeup chains
    CHAIN                                         WAKEUPS   EVENTS  LAST
    interrupt (hardirq) -> task:128611 -> task:128610       21       20  still open at the end
    interrupt (hardirq) -> task:128610                 20       20  ran
    interrupt (hardirq) -> task:128611                 20       19  still open at the end
    task:128607 -> task:128610                          1        4  ran
```

`a_timer_is_named_as_an_interrupt_and_a_thread_as_a_task` holds it to the
workload: at least twenty wakeups of the main thread named `interrupt
(hardirq)` and nothing else, at least twenty through the helper, the far end of
the helper's chain named as its own timer, and no chain calling a timer
`task:0 (idle)` -- which is what all twenty of the first kind read as before.

`examples/sched-wakeup.yml` declares its join now (`open-requests 280`), and
its header no longer says an interrupt's wakeup "is credited to whatever was
interrupted".

## 3. The match kinds the crossings asked for

### `woke` — a close paired with the next wakeup of its task (`open-requests 4968`)

```yaml
- {id: completion-released, from: io.latency, to: sched.wakeup, via: task,
   match: woke, woken: wakee_tid, waker: waker_tid, window: 1s}
```

A `from` event opens a state for the task it names; the first `to` wakeup of
that task ends it and is its match. `matched` and `unmatched` count the
completions -- one some wakeup followed, one none did -- because that is the
question, and a completion no wakeup followed is a reader that never slept, a
finding rather than a loss. Wakeups of a task with nothing waiting are the
rest of the scheduler's traffic and are not counted. With `waker`, each pair
is labelled by who issued the wakeup (an interrupt as the interrupt) and the
rows are folded by that label, each carrying in `change.delta` the summed time
from its completions to their wakeups. Evidence `WAKEUP`.

Ground truth, `every_completion_is_paired_with_the_wakeup_that_released_its_reader`:
1000 one-page `O_DIRECT` reads timed on the reader's own clock. Every read
blocks until its request completes, so each completion must pair with a wakeup
inside the same read:

```
1000 reads, 1000 reached the device; 1000 woken inside their read by the
reader's clock; the engine paired 1000, waiting 4229.1us in all against
8320.2us between each completion and its read's return;
by ["task:80140 -> task:160910 x1000"]
```

`task:80140` is a kworker: the completion is delivered by a worker, not by the
interrupt, which the crossing had only guessed from `/proc` after the fact.
The old `wakeup` join, still in `cross-io-one-read.yml` beside the new one,
pairs each request with the previous read's delivery; the two tables side by
side are the difference.

### `overlap` — a window across a poll's intervals, `TEMPORAL` (`open-requests 5032`)

```yaml
- {id: waiting-per-interval, from: proc.threads, to: sched.futex, via: task,
   match: overlap, value: total_ns, span: total_ns, stamped: start}
```

Each `from` reading covers the interval since the one before it of the same
key, or `[ts - interval_ns, ts]` when it says so itself; the first reading
with neither is a baseline. Each `to` event covers `[ts - span, ts]`, or
`[ts, ts + span]` with `stamped: start` -- which `sched.futex` and
`sched.offcpu` need, since they stamp a row when its first wait began -- or is
an instant when there is no `span`. A `to` event is credited to every interval
it overlaps, `value` times the share of it that overlaps; what lies outside
every interval is uncredited, not given to a neighbour. A drained row that
stands for several occurrences (`count`, `weight` or `blocks` above one) has no
one span to lay out, and is counted unmatched with its value uncredited.

Ground truth, `a_wait_across_several_readings_is_split_by_its_overlap_with_each`:
a thread waits four times for 250 ms on a condition nobody signals and prints
each wait's bracket on its own clock; the test computes each interval's share
from the brackets alone.

```
  (…2911532380, …3011541444]   100.009 ms long  credited  100.009 ms  the brackets hold  100.009 ms
  (…2511537077, …2611540600]   100.004 ms long  credited   99.969 ms  the brackets hold   99.991 ms
  (…3211518789, …3311543232]   100.024 ms long  credited   59.963 ms  the brackets hold   59.973 ms
  (…2211531876, …2311534670]   100.003 ms long  credited   40.430 ms  the brackets hold   40.439 ms
```

Every interval within 25 us of the brackets, against a 1 ms slack that names
what it is (the interpreter's call on each side of the syscall, twice, plus
the futex waits of the task no bracket holds, which the run reports). The
separation a naive join fails -- a 250 ms wait credited whole to one 100 ms
interval -- is asserted too.

What was asked and is not here: **`via` absent, for machine-wide state**
(`mem.pressure`). `Rule::via` is a required key and `src/validate.rs` checks
both sides carry it; both halves are filed.

### A part a join defines, and a traced span as the whole (`open-requests 4988`)

```yaml
decompose:
  - id: one-read
    whole: {from: io.syscall, value: total_ns}
    parts:
      - {state: device, from: io.latency, value: latency_ns}
      - {state: completion -> wakeup, from: completion-released, value: paired}
```

`value: paired` reads a part from a join: its `from` names one of the
session's `joins`, and the row is the distance between the two ends of every
pair that join made, summed per key, rated no better than the join. Only
`woke` can be read so, because only it ends a state on the event it is paired
with; the others are refused with that reason. The whole is `io.syscall`'s
`total_ns` for the task: since the eighth wave the syscall layer exists, and a
per-task sum of the syscalls that enclose every segment is the traced span the
crossing asked for. A per-span whole with the parts clipped to each span is
not written; nothing measured here needs it.

Measured on the same 1000 reads, through the library:

```
whole 23.59ms (brackets 24.06ms), device 10.92ms,
completion -> wakeup 4229.1us (Some(Wakeup)), residue 8439379ns
```

held to containment: the syscalls lie inside the reader's brackets, and the
device and the wakeup inside the syscalls, so the residue is never negative.
The residue is the filesystem, the submission and the run queue after the
wakeup.

**It needs two call sites to reach `omn`**: `main.rs` and
`triage/engine.rs` call `decompose` (R's), and `validate.rs` calls
`check_decomposition` (P's), neither of which is given the session's joins.
The library has `decompose_with` and `check_decomposition_in`; the three
one-line changes are filed. Until they land, a session with a `paired` part is
refused at validation with "it read the decomposition without the session's
joins", and the test computes it through the library.

### A block key the plug's clock cannot collapse (`open-requests 3713`)

No third component exists that both collectors read and that changes per
request -- the sixth wave's analysis stands: the tag is set at issue and
cleared at completion, the internal tag is unset under `none`, the sector
repeats on a rewrite, and `io_start_time_ns` comes from the same cached clock.
So the answer is in the engine rather than the key. Two lives of one pointer
under one scope are paired by order, which is sound -- a pointer is reissued
only after its completion, so the earlier life's completion arrives while the
earlier life is open -- and stops being sound only when a side lost the event
that opened the later life. The engine now counts each such replacement in
`states.scope_collapsed` and says `within_key: TEMPORAL` when there was one:
the key tied the events, and which life an event landed on was the clock's.
`no_completion_is_credited_to_a_request_that_had_already_finished` prints the
count and holds the rating to it.

## 4. Stacks a reader can read (`open-requests 710`)

`stack::demangle` reads Rust's legacy and v0 schemes with `rustc-demangle`,
in its alternate form (no hash, no crate disambiguator), and Itanium C++ with
`cpp_demangle`, which keeps the parameter types. `Frame::symbol` is the
demangled name and `Frame::mangled` the linker's, which `nm` lists. A name
neither reads whole is shown as the linker wrote it. Every encoder takes the
name from the frame, so `folded`, `speedscope`, `pprof` and the text all read
in the source's spelling at once.

Ground truth: a Rust fixture with modules and a method, and a C++ one with a
namespace, a method and a reference parameter, both sampled while they spin:

```
Rust: 399 of 399 samples ran through <omnnamed::chain::inner::Spinner>::c, each reading
  omnnamed::chain::outer::a -> omnnamed::chain::inner::b -> <omnnamed::chain::inner::Spinner>::c
C++: 400 of 400 samples ran through omn::fixture::Spinner::c(long), each reading
  omn::fixture::a(long) -> omn::fixture::b(omn::fixture::Spinner&, long) -> omn::fixture::Spinner::c(long)
```

with each frame's linker name found in the binary by `nm`. `rustc` 1.98
mangles v0 by default, which is why the eleventh finding's example was
`_RNvCsezHSzhuRag8_5chain4main`.

**Two crates, and why.** `rustc-demangle` is the Rust project's own, has no
dependencies, and is what `std`'s backtraces use; `cpp_demangle` is gimli's,
depends on `cfg-if` only, and is what `backtrace` and `pprof` use. Both
schemes are specified in full and each has corner cases a partial reader gets
wrongly and confidently -- the finding's own reason for not writing half a
demangler. Both were in the local registry; nothing was fetched.

## 5. A NUMA node key (`open-requests 1469`) — filed

`KeyValue` is J's and `KeyKind` is R's (`src/request.rs`), and
`KeyValue::kind()` is exhaustive, so the value cannot land before the kind.
The exact change for both files, and N's `mem.numa` line, is filed; the event
half is ten lines at the merge.

## 6. The tally's rows in stream J's files

| Row | Test | What changed |
|---|---|---|
| #10 | `cross_compute::a_half_blocked_task_decomposes_into_what_the_kernel_says_it_did` | The 0.35..0.65 wall-clock share is gone. The workload now reads the responder's `schedstat` while it still waits on the fifo; the driver's blocked time less its queue wait, plus its `run_delay`, must hold the responder's on-cpu time less 5 us a round. Measured 185.5 ms against 183.2 ms quiet, 252.7 + 18.4 against 238.1 ms beside 24 spinning shells. `short.is_none()` is now `short.is_some() == (dropped > 0)`. |
| #25 | `cross_io::the_crossing_sessions_run_and_say_what_they_found` | Landed by the coordinator (`863e537`): the sessions run in turn and a late attach is said rather than failed. |
| #26 | `join_engine::the_resident_size_decomposes_…` | The workload prints when its last free returned (`FREED`), and the test prints the last reading's offset from it and the shim's drops before asserting: a last poll before the end state, or a dropped record, is said with its numbers instead of failing the byte counts. |
| `5256` | the one-target comments | `tests/cross_io.rs` and `cross-machine-frequency.yml` were fixed by the coordinator; `tests/cross_machine.rs`, `cross-io-one-read.yml` and `cross-machine-reclaim-io.yml` said it still, and do not now. |

Rehearsed beside 24 spinning shells, which found three more in these files,
each the same shape -- a count against the workload's plan rather than what
the run reports:

- `join_engine::a_blocking_program_is_attributed_through_the_real_wakeup_collector`
  and `cross_compute::every_switch_of_the_task_is_credited_to_the_wakeup_that_released_it`
  required `matched + unmatched` to be every switch, and a switch stamped
  outside the wakeup source's window is counted in `outside_window` (616
  against 600 + 16). Both add `keyless` and `outside_window`.
- `join_engine::a_half_blocked_task_decomposes_as_the_kernel_says_…` and the
  second cross_compute test required 600 wakeups of the driver for 600 round
  trips. Under load an answer can reach the fifo before the driver has gone
  to sleep for it, and nothing needs waking: 596 and 523 were measured. Both
  are bracketed now by the driver's voluntary switches, the kernel's count of
  its blocks.

## 7. `omn-mcp` launches the sessions its operator wrote, and none other

Added mid-wave by the coordinator, with the user's approval. `run_session`
refused every session with a launch target (boundary 5), and the user wants an
agent to be able to benchmark their program through `omn-mcp`. The relaxation
keeps boundary 5's reason, which is that a model never chooses what code runs.

- **`--allow-launch DIR`**, meaningful only beside `--allow-run` (the server
  refuses to start otherwise). A launch target is accepted only from a session
  file in `DIR` or a directory under it, named in the call by `approved:
  NAME`. The file is read once, and the text that was checked is the text
  `omn` runs, from the file's own directory: program, arguments and relative
  paths are the file's, and a call carries only the duration and budget,
  which bound the run as before. A name that is absolute, holds `..` or `.`,
  or resolves outside `DIR` through a link is refused. A launching session
  inline or by `path` is refused as before, and the refusal names
  `--allow-launch`.
- **`list_approved`** lists the approved sessions, each with what
  `plan_session` says of it; a file there that is not approvable (a link out)
  is listed with why rather than hidden.
- **No benchmarking tool.** Repeated `run_session` calls on an approved
  session: the server does not summarise, the bench's verdict needs a quiet
  machine and a release binary the server cannot promise, and N documents keep
  each run's losses and cost where a spread would drop them.
  `docs/mcp.md` has the reasoning in full.

Ground truth, `only_a_session_its_owner_approved_launches_a_program`: a python
program that touches a marker, writes a file relative to where it runs, and
says how many descriptors it holds. Approved, it runs: the marker exists, the
relative file is in `DIR`, and the answer's `proc.fd` maximum is the
program's own count, with its losses and measured cost. Inline, by a path
outside `DIR`, by five escaping names and through a link out, it is refused
and nothing is written; without `--allow-launch`, even the approved session is
refused. `docs/mcp.md` has the flag, the rewritten section and the test row.

## Declared: edits outside stream J's paths

| File | Edit | Why |
|---|---|---|
| `src/encode.rs` | `MatchKind::Woke` and `MatchKind::Overlap` arms in `unmatched_reason` and `table_words`, four lines | A new variant of the enum J owns does not compile without them. The rendering the two kinds want is filed for R. |
| `Cargo.toml`, `Cargo.lock` | `rustc-demangle = "0.1"`, `cpp_demangle = "0.5"` | Section 4. Nobody's file; the brief asks for the crate to be justified, which it is above. |

## Handoff

- **Done, with scenarios**: `omn-mcp`'s approved launches (item 7), `woke`, `overlap`, the scope collapse, demangled
  stacks, and the ground truth of the two twelfth-wave landings. **Done in the
  library, waiting on two call sites**: the `paired` part (R's `main.rs` and
  `triage/engine.rs`, P's `validate.rs`). **Filed, not landed**: the NUMA key
  (R's `KeyKind`), `overlap` with no key (P's validator), the renderer's
  words for both new kinds and for the ladder (R).
- **The index rows** for the two new sessions:

  | Session | What it shows |
  |---|---|
  | `join-interrupt-or-task.yml` | Who released a thread, a timer or another thread: the interrupt named as the interrupt, at either end of a chain. |
  | `join-overlap-waits.yml` | Waits split across the 100 ms intervals of a poll by their overlap with each, at TEMPORAL. |

- **For `CATALOG.md`** (the join notation): match kinds `woke` and
  `overlap`; rule fields `woken`, `span` and `stamped: end|start`; the
  decomposition value `paired`; `states.scope_collapsed`.
- **For `DECISIONS.md`**:
  - *It amends the twelfth wave's "whether a model may run a session" row*:
    yes with `--allow-run`, and a session that launches a program only when it
    is a file of the directory the person starting the server passed with
    `--allow-launch DIR`, run as the file states it, from its own directory.
    The model picks which approved session and when, never what runs.
    Benchmarking is repeated `run_session` calls, not a tool of its own.
  - *A block request's scope is not made unique; two lives under one scope are
    paired by order and rated TEMPORAL within the key.* No component both block
    collectors read changes per request, and order is exact while neither side
    loses an event.
  - *A part of a decomposition may be read from a join, and only from a
    `woke` join*, the one whose pairs have a distance that is a duration.
  - *Symbols are demangled by `rustc-demangle` and `cpp_demangle`*, and a
    name neither reads whole is shown as the linker wrote it.
- **The suite**, on this branch with the capabilities, beside four other
  streams. Own files: `cargo test --lib join` (67), `--lib stack`, and
  `tests/{join_engine, cross_io, cross_compute, cross_memory, cross_machine,
  stack_capture, vertical}.rs` and `-p omn-mcp` all pass, and passed beside 24
  spinning shells. `cargo clippy --all-targets -- -D warnings` (both packages)
  and `cargo fmt --check` are clean. The last `make check` stopped at the
  session-index test, red for the two sessions above as expected. The
  `cargo test --workspace --no-fail-fast` after it: **1925 passed, 4 failed,
  8 ignored** -- the index test, and three outside this stream's paths, each
  passing when rerun alone once: `perf::tests::closing_a_group_gives_its_descriptors_back`
  (in the tally's notes), `scenario_mem::a_walk_that_fits_in_l1_is_never_timed_and_one_that_does_not_waits_on_memory`
  (12 of 30 loads answered by L1) and
  `scenario_sched::perf_counts_the_blocks_a_raw_tracepoint_counts_and_preemption_besides`
  (tally notes, twelfth wave). An earlier `make check` stopped at
  `collect::tests::weighing_a_run_costs_a_fraction_of_the_run` (tally #12),
  which passed alone.
- **Filed for other streams**: `docs/open-findings.md`, "the thirteenth wave,
  stream J": R (`decompose_with` at two call sites; the text for `overlap`,
  `woke` and the ladder; `KeyKind::Node`), P (`check_decomposition_in`; `via`
  absent for `overlap`), N (`mem.numa` keyed by node, after R).

---


# The eleventh wave — the engine's debts

| | |
|---|---|
| Delivered | `join::apply_within` and `Correlation::outside_window`; `StateStats::window_closed` and `Outcome::WindowClosed`; `Results::window`; `Part::via`; a filtered part that matched nothing is a zero; a key a source never saw is a zero only from a source that watched the whole run clean |
| Refused, new | a join from a source the session collects twice with different windows (`JoinError::WindowsDisagree`), at validation; a part read `via` a key its source does not carry |
| Tests | 7 unit tests in `src/join.rs`, 1 in `src/encode.rs`; scenarios: `tests/join_engine.rs` +1, `tests/aggregation.rs` +1, `tests/vertical.rs` 1 flipped and strengthened; `tests/runner.rs`'s ignored detach scenario passes (E's file: un-ignore at the merge) |
| Files | `src/join.rs`, `src/emit.rs`, `src/encode.rs`, `src/main.rs`, `tests/{join_engine,aggregation,encoders,vertical}.rs`, `examples/vertical-one-request.yml`; one line outside, declared below |
| Machine | 24 cpus, kernel 7.2.6, `CAP_BPF` and `CAP_PERFMON` on the binary, loaded by three other streams; no test here skipped on it |

## 1. A join across a detached source

**The defect** (stream H, measured in `tests/runner.rs`): `sched.wakeup`
detached at 1 s, `sched.switches` traced for the whole run, and a program that
sleeps 40 times before the detach and 25 times after it. The join said
**67 matched, 0 unmatched**. The last wakeup before the detach stayed open, and
every block after the detach was credited to it: a stale state standing in for
the wakeups the run had stopped watching.

**The fix**, in the shape stream H filed:

```rust
pub fn apply_within(
    rule: &Rule,
    events: &[Event],
    windows: &BTreeMap<String, crate::collect::Watched>,
) -> Result<Correlation, JoinError>;
```

`apply` is `apply_within` with an empty map, which means every source watched
the whole run. Only the `from` source's window changes the answer:

- **A `to` event outside the `from` window** (before the attach or after the
  detach) is counted in `Correlation::outside_window` and not looked up.
  Neither `matched` nor `unmatched` fits it. The run was not watching for
  what would have opened or closed a state, and a wakeup the run did not see
  is not a wakeup that did not happen. It is left out of the share, as
  `keyless` is.
- **States open when the `from` window closes end there**, counted in
  `StateStats::window_closed`, and their attributions end with
  `Outcome::WindowClosed` (printed `source detached`). This is kept apart from
  `expired`, which means a state outlived the rule's own window. The rule's
  expiry is applied up to the close first, so a state that had already expired
  is counted as expired. The lifecycle balance gains one term:
  `opened = closed + expired + overflowed + window_closed + still_open`.
- **A detach with nothing after it** still ends the states. When the `from`
  window closed before the latest window in the map closed, the run went on
  without it, and whatever it left open ends at its close. When the source
  watched until the end, a state open at the end is `still_open`, as before.
- **Two requests of the `from` source with different windows** are the one case
  a map keyed by source cannot express. `check_in` refuses the rule by name at
  validation (`JoinError::WindowsDisagree`, naming both windows). It does not
  take the union, because a union would claim the source was watching while one
  of the two requests was detached. Two requests with the same window are
  accepted, and `main.rs` merges their collected spans by union, because they
  differ only in the instant each was attached.

`main.rs` builds the map from `Results::window` (§2) after aggregation rather
than from the collections, so the call site does not need to keep the
collections alive.

The text form, under the share:

```
  25 more `sched.switches` events fell outside `sched.wakeup`'s window, when it was not
  attached: neither matched nor unmatched, since the run was not watching for what would
  have opened or closed a state for them
  states: 42 opened, 40 closed, 0 still open at the end, peak 2 held at once
  2 more ended when `sched.wakeup` was detached: nothing after it was credited to them
```

**Measured.** Six runs of the ignored scenario on this branch:

| Run | matched | unmatched | outside_window | window_closed |
|---|---|---|---|---|
| 1-6 | 43, 44, 42, 41, 43, 44 | 0 every time | **25 every time** | 2 to 4 |

The scenario's ground truth holds on every run: the 25 blocks after the detach
all fall outside the window, none of them is matched, and none is counted as
unmatched. `matched` is the 40 sleeps plus the extra wakeups of the program's
start and gate, and plus whatever else the machine's load woke it for. The
scenario bounds that at `FIRST + 3`, and one run of six gave 44 (44 wakeups
inside the window, so 44 states opened). That bound measures the machine, not
the engine. The replacement assertion, `matched < FIRST + SECOND`, is filed for
the coordinator in `docs/open-findings.md`. It keeps the 25-block separation
the scenario exists for.

**The other match kinds.** A unit test runs `eq` and `interval` across a detach.
A key's state, or a range allocated before the detach, takes nothing after it,
because no free or replacement the run could not see can end it. `between`
goes through the same path: a reading outside the window is outside, and a
change waiting for a reading when the source comes off is ended by the detach.

## 2. The window on `Results`

```rust
/// When this request was watching, inside the run: its own window when the
/// request carried one, the run's otherwise.
#[serde(default)]
pub window: crate::collect::Watched,
```

It is copied from `Collection::window` in `aggregate`. It is in every JSON,
YAML and TOML document, in the `ndjson` header record, and in the `chrome`
request record. `observed_ns` is still the span, and every rate is still over
it. The window says where that span lay. The text form still states the window
through the notes the runner writes (`watched only from 1.500 s into the
run...`), and does not add a second line saying the same thing.

**Measured** (`tests/aggregation.rs`,
`a_result_says_where_in_the_run_its_request_watched`): two polls of one
program, one given `window: 300ms..`. The session supplies the ground truth.
The unwindowed request opened at the run's open (offset 0), the windowed one no
earlier than 300 ms into it, and both closed inside the run. On both,
`observed_ns == closed_ns - opened_ns`. How long the attach took under load is
not asserted.

## 3. A row read through another key is rated by it

`net_rx` and `net_tx` in the vertical case were printed `IDENTITY` and should
have been `KEY`. Neither runs in the serving thread, and what ties them to the
request is the socket cookie. That was an over-rating, the one direction the
evidence ladder must never err in. Stream N's signature:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub via: Option<KeyKind>,
```

A row with `via` is rated `rate(by).min(rate(via))`. `rate(req)` is `IDENTITY`
and every other key is `KEY`, the same ladder a join uses. `check_decomposition`
refuses a `via` the part's source does not carry ("a row cannot be rated by a
key its events never had"). `net.request` declares `sock`.

`examples/vertical-one-request.yml` takes `via: sock` on the two parts, and its
comment says why. `tests/vertical.rs::the_network_rows_are_read_by_the_socket_the_server_accepted`
now asserts `KEY` for both. It also checks what the brief made the bar: rating
a row changes no amount. The six rows still tile the request's span to the
nanosecond, and still add up to the latency the client measured on its own
clock, within the 10 % the roadmap's criterion allows. Measured on this branch:
all 15 vertical scenarios pass.

## 4. A filtered part that matched nothing is a zero

**The defect** (stream T): a part with a `filter` that no event passed read
`not read`, with the message "no `sched.offcpu` event carried both `total_ns`
and `queued_ns`". That row took `parts` and `RESIDUE` with it. The cause is in
`Readout::new`: it asked whether the source carries the attribute *after* the
filter, so a filter that kept nothing looked like a source that could not
supply the value.

**The fix.** The readout now asks three questions of the source's events
*before* the filter: whether any event carries the attribute, which keys the
source produced events for, and, for each key, which events the filter kept.
The rows then read:

| The source... | The row |
|---|---|
| was not collected | `not read`: the run collected no `<source>` |
| carried the attribute on no event at all, before any filter | `not read`, as before |
| had events for this key, and the filter kept some | the amount, as before |
| **had events for this key, and the filter kept none** | **`0`**, note `no event of this key passed the filter` |
| had no event for this key, watched the whole run, lost nothing | `0`, note `no <source> event of this key, from a source that watched the whole run and lost nothing` |
| had no event for this key, and lost events, watched only part of the run, or cannot say when it watched | `not read`, with that reason |

**The decision on the unfiltered case**, which the brief left to the engine:
a key the source never produced an event for is a zero **only from a source
that watched the whole run and lost nothing**. Such a source did look, and an
absent row there is a measurement. A cpu-bound task that never blocked has no
`sched.offcpu` row, and its blocked time is zero, not unknown. A source that
lost events may have lost exactly that key's events. A source that a window
narrowed was not watching for part of the run. A result that does not say when
it watched (a `Watched` left at its default: a document written before this
wave, or a hand-built result) is unknown, and unknown never counts as
available. "The whole run" is judged from the results themselves: attached,
opened at the run's open, and closed no earlier than the latest close of any
result.

This also closes a silent path that nobody had filed. Before, a key absent from
a source that carried the attribute for other keys read `0` with no note,
whether or not the source was lossy or windowed. A zero from a lossy source now
reads `not read`, with the reason.

**Measured** (`tests/join_engine.rs`,
`a_filtered_part_that_matched_nothing_is_a_zero_and_the_table_adds_up`): the
program is `/bin/sleep 0.3`, so no block of it can outlast its life. That is
known from the program, not from the tool. A part filtered to `total_ns >
1000000000000` (1000 s) keeps nothing:

```
    blocked      sched.offcpu       total_ns     traced   KEY             300.05ms        exact  99.6 %
    impossible   sched.offcpu       total_ns     traced   KEY                  0ns        exact   0.0 %
    parts                                                                 300.05ms
    whole        the program's life gate to reap lifetime -               301.17ms   +-889.26us
    RESIDUE      whole - parts                                             +1.12ms   +-889.26us
      impossible: no event of this key passed the filter
```

The test requires the zero, the note, `parts` equal to the unfiltered row, and a
residue that is read. It also requires the unfiltered row to lie between the
300 ms the program asked for and the life the tool measured. The unit tests
cover every other line of the table above, one per reason.

## What changed for readers of a document

- `window` on every result, and `outside_window` and `states.window_closed`
  on a correlation (the last two appear only when they are not zero).
- An attribution's `outcome` can be `window_closed`.
- A decomposition row can now be `0` with a note where it used to be
  `not read`, and `not read` where it used to be a silent `0`. The second
  case is only a lossy, windowed or unplaced source.

## Declared: the one edit outside stream J's paths

| File | Edit | Why |
|---|---|---|
| `src/triage/latency.rs` | `via: None,` in the one `Part` literal | `Part` gained a field, and the branch does not compile without it. Stream H did the same with `window: None` last wave. Told to stream E in `docs/open-findings.md`. |

`cargo fmt` also rewrites `tests/scenario_runtime.rs:929` on the base commit.
That file is not this stream's, so the branch leaves it as it was. Filed for the
coordinator.

## Handoff

- **Done**: all four requests work with their scenarios. The ground truth of
  the first, the ignored scenario in `tests/runner.rs`, gives 25 of 25 outside
  the window on every run. **It is ready to un-ignore**, with the one bound
  change above.
- **The index rows**: none. This stream added no session, and the one session
  it changed (`vertical-one-request.yml`) is already in both indexes.
- **The ignored tests this half makes ready**:
  `tests/runner.rs::a_join_across_a_detached_source_does_not_count_what_fell_after_it_as_a_loss`.
- **For stream E's escalation**: joins across a source attached or detached at
  the escalation are now honest, through `apply_within`, which the runner's
  call site already uses. E does not need to avoid any join.
- **The suite**, on this branch with the capabilities, while three other
  streams loaded the machine. Own tests: `cargo test --lib join` (62),
  `--lib encode` (41), `--lib emit` (37), and `tests/{join_engine, aggregation,
  encoders, vertical}.rs` (13, 12, 13, 15) all pass. `cargo clippy
  --all-targets -- -D warnings` is clean. `cargo fmt --check` flags only
  `tests/scenario_runtime.rs` (the base's, above). One `make check` stopped at
  `perf::tests::an_ibs_sample_says_which_level_answered_the_load` ("too few
  samples to say anything"). The one `cargo test --workspace --no-fail-fast`
  that followed gave **1753 passed, 2 failed, 9 ignored**, and it failed a
  different pair:
  `scenario_io::the_page_cache_sessions_say_what_their_headers_predict` (the
  warm read found 256 of 4096 pages evicted) and
  `scenario_net::every_datagram_sent_to_a_closed_port_is_a_drop_with_one_reason`
  (21 of 512 drops were attributed to the closed port, among 2045 drops from
  other traffic on the machine). All three are outside stream J's paths and
  none reads the engine. **Each passed when rerun alone, once.** None is in
  the flaky tally by name, so they are named here for it.

---

# The eighth wave — `TEMPORAL`, the decomposition, the chains

Four of the seventh wave's crossings computed their answer in a test because
the engine could not say it. This stream made the engine say it: a match kind
whose evidence is `TEMPORAL` by construction, a `decompose:` block with a
signed residue, wakeup attributions ranked by chain, and two refusals. What
follows is what landed, what each piece was checked against, and what is left
with the reason it was left.

| | |
|---|---|
| Sessions | `examples/join-between-residency.yml`, `examples/join-decompose-memory.yml`, `examples/join-decompose-compute.yml` |
| Tests | `tests/join_engine.rs` — seven new: three against a workload's own report, three refusal tests and one format claim; 24 new unit tests, 19 in `src/join.rs` and 5 in `src/encode.rs` |
| Files | `src/join.rs`, `src/request.rs`, `src/encode.rs`, `MatchKind` in `src/catalog.rs`; the edits outside that list are declared at the end |
| Machine | 24 cpus, kernel 7.2.6, `CAP_BPF` and `CAP_PERFMON` on the binary; no test here skipped on it |

---

## What `TEMPORAL` means now

The brief made this stream the one that decides what the last rung of the
ladder means, so here it is, as the engine applies it:

> **`TEMPORAL` is a correlation whose only ground is the clock.** Two things
> happened in the same interval, or one was the most recent before the other,
> and nothing either event carries says they are the same thing.

It is not a synonym for the rungs above it, and the tests hold that as a
separation rather than as a label (see the decoy below). Where each level on a
joined result now comes from:

| Result | Evidence | Why that and not more |
|---|---|---|
| `between`, always | `TEMPORAL` | the only thing tying a reading's change to a state is that both happened in one interval |
| `eq` on a scoped key (`blk`) | `TEMPORAL` | the pointer alone is shared; which life of it an event lands on is whichever held the address last |
| `eq` that replaced states | `KEY`, with `within_key: TEMPORAL` | the key says whose; which of that key's successive states got the event, the clock decided |
| `wakeup` | `WAKEUP` | unchanged |
| `eq`, `scoped_eq`, `interval` otherwise | `KEY` | unchanged |
| `eq` on `req` | `IDENTITY` | unchanged |
| a decomposition row | `KEY` | its events carry the table's key; nothing propagated an identity between rows |
| a decomposition's window | none | a bracket, not a join |

`within_key` is set only for `eq`. `scoped_eq` replaces states too, and chooses
among them by the scope, which is what it exists for; `interval` chooses by the
address, which is a key. It is never set when the evidence is already
`TEMPORAL`, because it would say nothing the evidence does not.

---

## 1. `between`

```yaml
- id: residency
  from: mem.heap.alloc
  to: mem.rss
  via: task
  match: between
  value: bytes          # the attribute of a `to` reading that is the total
  reading: level        # or `delta`; required when `to` is polled
  opens: "op == 'alloc' && bytes >= 1048576"
```

Each `to` reading's change since the previous reading with the same `via` key
is credited to the `from` states that opened or closed in that interval.

- **A state is one `from` event waiting for the next reading of its key.** It
  opens when the event arrives and closes when a reading takes it into an
  interval, so `opened = closed + expired + overflowed + still_open` holds as it
  does for the other four matches, and the same window and capacity bound it.
- **The attributions are intervals, not states.** An interval in which four
  ranges opened and the total rose by 32 MiB is one row: `+32 MiB, 4 opened, 0
  closed`. Splitting the 32 MiB four ways would be inventing a precision the
  clock does not have.
- **Every reading is accounted for, as one of three things**: the first of its
  key, with nothing to differ from; a change in an interval where a state opened
  or closed (`matched`); or a change in an interval where none did. The last two
  are summed separately (`readings.credited`, `readings.uncredited`), because a
  resident size moves for reasons no allocation tracker sees, and hiding that
  would make the credited share look like the whole.
- **`reading: level | delta`** is required on a polled `to`, because the catalog
  does not say which a poll is. `mem.rss` is a level, where what happened is the
  difference of two readings. `cpu.oncpu_time` is a delta, where what happened is
  the reading itself. The difference of two deltas is a quantity of nothing, so
  the tool does not guess. A snapshot is a level, and a traced or sampled `to`
  is refused.

**Refused before the run**, by `join::check_in`, which the validator now calls:

```
error: rule `moved` matches by `between` onto `mem.faults`, which this session collects in
`trace` mode: `between` credits the change between two readings of a total, and a `trace`
event has no reading before it to differ from. Collect `mem.faults` with `poll` or
`snapshot`, or join it by a key
```

### Measured — `examples/join-between-residency.yml`

The program has four phases 400 ms apart and prints each with the
`CLOCK_MONOTONIC` instant it began, what it wrote, and how many large blocks it
allocated. Three runs:

| Phase | What the program did | Credited interval, run 1 / 2 / 3 | Opened |
|---|---|---|---|
| kept | four 8 MiB blocks, written | +32.1 / +32.1 / +32.1 MiB | 4 |
| returned | four more, written | +32.0 / +32.0 / +32.0 MiB | 4 |
| freed | those four freed | not credited: −32 MiB with nothing opening | — |
| **decoy** | **one 1 MiB block, never written**, and 16 MiB `mmap`ed and written | **+16.0 / +16.0 / +16.0 MiB** | **1** |

**The decoy is the test that `TEMPORAL` is not a synonym for `KEY`.** The
allocation tracker sees the unwritten block and not the mapping. `between`
credits the interval's 16 MiB to the block, which holds one page, sixteen times
more than it can hold, because that is what crediting by the clock does. The
correlation says `TEMPORAL`, and that is the only thing between a reader and
the belief that the block cost 16 MiB. The test requires the credit, and
requires the level.

The same session's `eq` on `task` says:

```
join by-task  mem.heap.alloc -> mem.rss  evidence KEY, TEMPORAL within the key
  the key says whose each event is; which of that key's successive states it went to, the
  clock decided: 6219 states were replaced by the next of the same key
```

which is D1 of `docs/scenarios/cross-memory.md`, fixed in the form P1 asked for.

**Two limits, both printed rather than hidden:**

- **The freed phase is credited to nothing.** A free carries no size (the shim
  writes `bytes=0` on a free), so no `closes` predicate can pick the four large
  frees out of the thousands the interpreter makes. A `closes` that matched
  every free would put a state change in every interval. The −32 MiB is
  reported as a change with nothing opening or closing around it, which is true
  of this join. The request is below.
- **A phase can straddle a reading.** Part of its change then lands in the next
  interval, where nothing opened. One poll is the resolution of `TEMPORAL`
  evidence here. The test adds the reading after each interval back in, and
  asserts the sum, rather than asserting which way the race went.

---

## 2. The decomposition

```yaml
decompose:
  - id: where-the-time-went
    whole: window                       # or {from: <source>, value: <attr>, reading: level|delta}
    parts:
      - {state: on-cpu,   from: cpu.profile,   value: samples}
      - {state: runnable, from: sched.runqlat, value: total_ns}
      - {state: blocked,  from: sched.offcpu,  value: total_ns}   # less: queued_ns, once it exists
    by: task                            # the default
    top: 2                              # keys to report, heaviest first
```

It is the shape both P2 and P3 wrote down: a whole, a list of parts, and a
**signed residue that is never rounded away**. The output is one table per key,
and every row carries:

| Field | What it is |
|---|---|
| `kind` | `sampled` (a count times a period), `traced` (every occurrence summed), `polled` (short by what came after the last reading), `counted`, or `window` (a bound, not a measurement) |
| `evidence` | `KEY` for a row whose events carry the table's key; absent for the window |
| `resolution` | one period for a sample count, zero for a traced sum, one poll for a polled delta in ns; **absent** where it cannot be stated in the decomposition's unit, which is every polled level |
| `short` | the source's `emit::Shortfall` when the run was lossy, and the amount is then printed `>=` |
| `amount` | **absent**, with `missing` saying why, when no event carried the attribute. A row that read as zero would be a measurement of nothing, and a missing row takes the residue with it |

A part's `value` is an attribute summed (a delta) or taken last-minus-first (a
level), or `samples`: the count of the source's events times the period of the
**served** request, since a budget can lower a sampling rate. `less:` subtracts
a second attribute. `filter:` selects which of the source's events a row reads.
The memory session uses it because the first poll of a launched program can land
before it exec'd, and read this tool's own forked image.

**Refused before the run**, by `join::check_decomposition`:

- a part or whole whose source the session does not collect, or collects twice
  (a row has to come from one request);
- a part whose request names a **different target** than the whole's (or than
  the first part's, when the whole is the window): *"a row about another target
  is a row about a different program"*;
- `samples` on a source that is not sampled, or `samples` with `less`;
- a polled row that does not say `reading: level` or `reading: delta`;
- parts in different units: *"added together they are a quantity of nothing"*;
- a part whose source does not carry the `by` key.

### Measured — the memory crossing's table, by the tool

`examples/join-decompose-memory.yml`, the crossing's "did it stay" workload
unchanged:

```
decompose where-the-memory-went  per task, in bytes

  task:333805
    STATE        SOURCE             VALUE        KIND     EVIDENCE          AMOUNT   RESOLUTION   SHARE
    live         mem.heap.retained  live         polled   KEY             48.0 MiB            -  74.9 %
    retained     mem.heap.retained  bytes        polled   KEY             15.0 MiB            -  23.4 %
    parts                                                                 63.0 MiB
    whole        mem.heap.retained  rss          polled   KEY             64.1 MiB            -
    RESIDUE      whole - parts                                            +1.1 MiB            -
```

The crossing's own figures were rss +64.09, live +48.01, retained +14.98 and a
residue of +1.10 MiB, which is what this prints on each run taken here. The test
holds the rows to the program, not to the crossing: live to what the program
still holds (48 MiB), to sixteen pages; retained to the whole-page bytes the
program prints, to sixteen pages; and the residue between −16 pages and the
program's predicted part (1020 KiB of freed bytes on pages shared with a live
block) plus one huge page.

### Measured — the compute crossing's table, by the tool

`examples/join-decompose-compute.yml`, the half-blocked ping-pong:

```
decompose where-the-time-went  per task, in ns

  task:335493
    STATE        SOURCE             VALUE        KIND     EVIDENCE          AMOUNT   RESOLUTION   SHARE
    on-cpu       cpu.profile        samples      sampled  KEY             202.61ms     +-1.00ms  45.5 %
    runnable     sched.runqlat      total_ns     traced   KEY               1.17ms        exact   0.3 %
    blocked      sched.offcpu       total_ns     traced   KEY             171.99ms        exact  38.6 %
    parts                                                                 375.77ms
    whole        the run's window   observed_ns  window   -               445.18ms            -
    RESIDUE      whole - parts                                            +69.41ms     +-1.00ms
      on-cpu: 202 samples x 1.003 ms
      whole: an upper bound: the window runs to the last drain, and the life of what is
      decomposed may end well before it
```

Against the kernel's own accounting of the same task, printed by the workload
and read by nothing in the tool, on two runs: on cpu 202.6 / 195.6 ms against
`schedstat`'s 199.4 / 193.1; runnable 1.17 / 1.25 ms against `run_delay`'s 0.03
/ 0.14 ms. That is finding 7 of `docs/scenarios/cross-compute.md`, a few µs per
wait on an idle cpu, inside its 5 µs-per-wait slack. The parts add to 375.8 /
361.2 ms against the workload's own clock of 369.6 / 355.7 ms. **The residue
against the window is an upper bound, and against the clock it is negative, by
about 6 ms.** That is the double count finding 1 names (the queue wait after
each wakeup is in both `blocked` and `runnable`), plus the interrupts a
`cpu-clock` sample counts and `schedstat` does not (finding 2). The tool prints
what the rows say and does not guess the overlap. What would remove it is below.

---

## 3. Wakeup attributions, ranked by chain

`assemble` folds a wakeup join's attributions by chain before it sorts and
truncates: events summed, wakeups counted (`wakeups` on each row), earliest
wakeup to latest end, and the outcome of the latest wakeup. The same compute
run:

```
  heaviest wakeup chains
    CHAIN                                         WAKEUPS   EVENTS  LAST
    ... -> task:335536 -> task:335493                 601      603  ran
    task:335533 -> task:335493                          2        2  ran
```

Before the fold, the top of this table was whichever single wakeups preceded a
preemption: 5, 4 and 3 events each, out of 1230 states. The test requires the
top chain to end `task:<responder> -> task:<driver>`, with the responder's pid
printed by the workload itself. It also requires 600 ± 3 wakeups, the number of
round trips, and every other chain under a tenth of that.

`still open at the end` replaces `never ran` for a wakeup state nothing ended.
A chain with events attributed to it did run.

---

## 4. The two refusals

**A wakeup join onto a drained total** is refused before anything is attached:

```
error: rule `released` matches by `wakeup` onto `sched.offcpu`, whose events are totals
drained from the kernel: each stands for many occurrences, and a wakeup can release only
one. Every block in a drain would be credited to whichever wakeup came last before it, and
reported as WAKEUP. Join onto a source with one event per occurrence, such as
`sched.switches`
exit 2
```

The catalog says which sources drain: `Backend::drained`, a new field, is set
on every backend of `sched.offcpu` and `sched.runqlat`, and
`Entry::always_drained` is true when every backend of a chain has it. A chain
where one backend streams per occurrence can still serve the join, and which
backend a run gets is the resolver's call after validation. For a source the
catalog does not mark, the engine refuses on the first `to` event that carries
`weight`, `total_of` or `window_ns`: every collector that drains marks its rows
with at least one. That refusal comes after the run, and the catalog flag is
what moves it before. The request to flag the other drained sources is below.

The P2 alternative, a weight the engine counts `matched` in, was not taken.
It makes the count right and the attribution still wrong, as P2 said.

**`eq` on a scoped key is rated, not refused.** It reports `TEMPORAL` and says
why under the join:

```
  `eq` compares a recycled identifier without the scope that says which life of it is meant,
  so an event lands on whichever life held it last: two events at one address around the
  same time, which is TEMPORAL. `scoped_eq` tells the lives apart
```

Refusing it would remove the one session that shows what the scope prevents
(`examples/cross-io-stale-pointer.yml` runs both on purpose, and the bare join
credits 11 496 completions to a stranger). The invariant is *refused, never
degraded silently*. A correlation that states its evidence and the reason for
it is not silent, and `TEMPORAL` is what a bare recycled pointer establishes.

---

## Scenarios

| Test | Session | Checked against |
|---|---|---|
| `between_credits_what_moved_together_and_says_temporal_even_when_it_is_wrong` | `join-between-residency.yml` | the program's own phase report: instant, bytes written, blocks allocated; the decoy's unwritten block |
| `the_resident_size_decomposes_into_live_and_retained_with_the_residue_the_program_predicts` | `join-decompose-memory.yml` | what the program holds, the whole-page bytes it freed, and the shared-page bytes it predicts |
| `a_half_blocked_task_decomposes_as_the_kernel_says_and_its_releaser_heads_the_chains` | `join-decompose-compute.yml` | `/proc/self/schedstat`, the workload's clock, the responder's pid, 600 round trips |
| `a_wakeup_join_onto_a_drained_total_is_refused_before_anything_is_attached` | written | exit 2, the reason, the source to use |
| `between_is_refused_onto_what_is_not_a_reading_and_onto_an_unstated_poll` | written | exit 2 twice, and exit 0 once the reading is stated |
| `a_decomposition_that_cannot_mean_anything_is_refused_before_the_run` | written | exit 2 for an uncollected part and for a part about another target |
| `the_formats_that_claim_to_carry_correlations_carry_them_and_the_others_refuse` | none | `OutputFormat::carries_correlations` crossed against both encoders, every format |

Each tolerance is the reference's resolution, never a calibration to this machine:

- a huge page plus sixteen pages for resident sizes, the bound the memory
  crossing uses;
- 5 % plus three periods for a sample count;
- 5 µs per wait for `run_delay`.

Only the memory tests depend on the machine's THP mode, and only through that
huge page.

**What cannot be checked here.** Everything eBPF needs the capabilities. On a
binary without them the compute test skips, loudly, with what it would have
checked; the two python tests skip on a machine without `python3`. **The
residue of a time decomposition against the task's exact life** cannot be
checked until the lifetime is an output, so the test asserts only what the
window's bracket can prove: the parts are at least the task's own clock, and at
most the window plus what the runnable row can have counted twice.

---

## Declared: edits outside stream J's paths

The user chose minimal declared edits over dormant features, so that every
feature works end to end and `make check` stays clean. Every edit outside
the list, and why:

| File | Edit | Why |
|---|---|---|
| `src/main.rs` | `decompositions: Vec::new()` in `RunOutput`; a dozen lines after the joins calling `join::decompose` with the served requests | the joins are applied there; without it `decompose:` would parse and never run |
| `src/validate.rs` | `check_joins` calls `join::check_in` per rule and `join::check_decomposition` per decomposition | *refused at validation* is where the brief puts both, and `join::check` was never called before the run at all |
| `src/catalog.rs` | `Backend::drained` and `Entry::always_drained`, beside the `MatchKind` this stream owns | the drained refusal needs the catalog to say which sources drain |
| `catalog/sched.yaml` | `drained: true` on the three `sched.offcpu` and two `sched.runqlat` backends | the two sources the compute crossing measured; no stream owns `sched.yaml` this wave |
| `src/format.rs` | `OutputFormat::carries_correlations` | the brief: a decomposition only some encoders render has to say so there |
| `tests/encoders.rs` | `decompositions: Vec::new()` in two `RunOutput` literals | the struct gained a field |
| `tests/cross_memory.rs` | D1's pinning test asserts `within_key: TEMPORAL`, renamed `..._says_temporal_within_it` | its own message asked the fixer to flip it |
| `docs/scenarios/cross-memory.md` | the one line naming that test | the name changed |
| `tests/cross_compute.rs` | finding 5's test now requires the refusal and still measures blocks per drained event; renamed `..._is_refused_because_each_event_holds_many_blocks` | its own doc said it fails the day the engine refuses the rule |
| `examples/cross-compute-drained-wakeup.yml` | the refused join moved into a comment with the refusal text | the session must still validate; the test reads the rule back from the comment |
| `tests/cross_io.rs` | the bare pointer's evidence is `TEMPORAL` | defect 3, rated lower as P3 offered |
| `examples/cross-io-stale-pointer.yml` | the comment's `evidence KEY` for the bare join, and the paragraph saying the engine calls both KEY | it said what the engine no longer does |

Not edited, though `make check` wants them: **`examples/README.md` and
`GUIDE.md`**. The brief gives both to the coordinator by name, and says the
index test is theirs to satisfy at the merge.
`the_session_indexes_hold_exactly_the_sessions_on_disk` is therefore red on
this branch, for the three new sessions and nothing else. The rows, ready to
paste in both files:

```
| `join-between-residency.yml` | What moved the resident size, asked by key and by the clock -- and a decoy the clock gets wrong, at TEMPORAL. |
| `join-decompose-memory.yml` | The rise in resident size split into live and retained bytes, with a signed residue the program predicts. |
| `join-decompose-compute.yml` | One task's wall clock split into on-cpu, runnable and blocked, each row's kind and resolution, and its releasers ranked by chain. |
```

The scenario documents' prose is not updated beyond the one renamed line.
`docs/scenarios/cross-memory.md` D1, `cross-compute.md` findings 5 and 6 and the
`never ran` wording, and `cross-io.md` defects 3 and 4 are now fixed, and each
document still describes them as open. That is the coordinator's to reword, or
the next crossing's.

---

## Not done, and why

- **P3's `woke` match kind** (a close paired with the next wakeup of its
  task). It is not in this wave's brief, and it is a sixth match kind with its
  own semantics for `unmatched`. It needs its own scenario, which P3's I/O
  crossing already has the workload for.
- **P4's `overlap`**, a `to` window split across every open state by the share
  it overlaps. `between` covers the case three crossings asked for: a polled
  total credited to what moved with it. Proportional shares are a different
  claim, and less honest at this level of evidence: splitting by overlap
  asserts a rate the clock does not know. If the machine crossing still needs
  it once `between` is used, it is a sixth kind with its own test.
- ~~**`whole: lifetime`.**~~ **Landed at the merge**, the one `Span` variant
  this said it would be, once stream R's lifetime reached the results. It is
  exact at the gate and bracketed at the exit, and the bracket is in the
  residue's resolution. On the compute session it took the residue from
  **+97.29 ms** -- almost all of it the drain arriving late -- to **-0.55 ms
  against a resolution of 1.58 ms**. Refused at validation in a session that
  launches nothing.
- **The `interrupted` row** (compute finding 2) needs the kernel's own on-cpu
  figure as a source; nothing reads `schedstat` today.
- ~~**The double count in a time decomposition**~~ (compute finding 1) —
  **done, with no engine change**, exactly as this said: `sched.offcpu` carries
  `queued_ns` since the coordinator's pass in this wave, and the session's
  `blocked` row is `{value: total_ns, less: queued_ns}`.
- **A decomposition in `folded`, `speedscope`, `pprof` or `chrome`.** A profile
  has no place for a residue. `carries_correlations` says so, `chrome` names the
  decompositions in its metadata the way it names joins, and the other three
  drop them as they already dropped joins.

## Needs from another stream

Written out with signatures under the eighth wave's stream J heading in
`docs/open-findings.md`: the index rows above; `drained` in `CATALOG.md` §1;
`drained: true` on the other drained sources; a free that carries its size;
`Span::Lifetime` once stream R's lifetime lands; and the manual page.

## Handoff

- **Done**: everything the brief gave this stream works with its scenarios:
  `between`, `within_key`, the `decompose:` block, chains, and both refusals.
- **The suite**, after the last edit, on this machine with the capabilities:
  `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` are clean.
  `cargo test --workspace --no-fail-fast` gives **1437 passed, 1 failed**, and
  the failure is `the_session_indexes_hold_exactly_the_sessions_on_disk`, naming
  exactly the three sessions above. `make check` stops at that test, because
  `cargo test` stops at its first failing binary, which is why the numbers are
  from `--no-fail-fast`.
- **Seen on the way, not this stream's**: one `make check` run failed in
  `collect::tests::what_a_run_reports_spending_is_the_cpu_it_actually_burned`,
  with "the collector was asked to spin for 200 ms and did not". That is the
  under-load flake `docs/open-findings.md` already names, in
  `src/collect/mod.rs`, which stream R owns. It passed three times out of three
  alone, at a load average of 6.
- **Re-run after a reboot** (load average 12, other streams building): fmt and
  clippy clean; `--no-fail-fast` showed five failures besides the index test.
  Three pass alone, every time: `off_cpu_contains_the_queue_wait_that_runqlat_also_counts`
  and the two `scenario_net` tests, which are under-load flakes like the one
  above. The fourth, `aggregation::two_emits_of_one_request_describe_the_same_events`,
  fails every run **on `main` as well**, with the same numbers: it compares
  `mean * count` to a sum with `assert_eq!` on `f64`, and they are one ulp
  apart (`216167983.0` against `216167983.00000003`). It is filed under stream
  J's heading in `docs/open-findings.md` for whoever owns `tests/aggregation.rs`.
