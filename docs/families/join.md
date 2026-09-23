# The join engine and the decomposition — stream J, eighth wave

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
