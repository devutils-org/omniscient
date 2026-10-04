# Omniscient — ROADMAP

Implementation roadmap. Meant to be executed by an agent, step by step,
**without skipping a milestone**.

Related documents: `CATALOG.md` (the vocabulary of sources).

---

## Guiding principle

The ordering is not "simplest to hardest". It is **"what is expensive to change
later"**.

A collector can be rewritten in a day. A change to the event model or to the
clock domain has to be paid for across every existing collector. So: model first,
skeleton next, breadth last.

**Milestone exit rule**: every phase has measurable criteria. Until they are met,
do not start the next one. A milestone that is "almost done" and gets paired with
a new worksite never finishes.

---

## Standing anti-goals

To be recalled at every phase — these are the project's natural drifts:

- ❌ **Do not wrap `perf`, `bpftrace` or `bcc`.** Omniscient talks directly to
  `perf_event_open`, `bpf()`, procfs/sysfs, netlink. Shelling out means the
  project failed.
- ❌ **Do not implement breadth-first.** Seven half-finished collectors and zero
  correlation is the default outcome if you follow the catalog in order.
- ❌ **Do not hard-code source availability.** Everything goes through machine
  introspection.
- ❌ **Do not degrade silently.** An unavailable source, a filter that was not
  pushed down, a fallback backend, lost events: all of it must surface.
- ❌ **Do not chase bit-exact determinism.** That lives outside the kernel-level
  core (it is dynamic binary translation). An accepted boundary, to be documented.
- ❌ **Do not promise the inside of NVIDIA GPUs.** CUPTI is closed, there is no
  kernel path. An accepted boundary.
- ❌ **Do not tune anything.** No `madvise`, no `sched_setaffinity`, no queueing
  discipline, no cgroup limit, and nothing that modifies the target in order to
  read it. A tool that measures and changes in the same run produces numbers
  nobody can interpret. An accepted boundary, and the one most likely to be
  proposed as a feature.

---

## Phase 0 — Decisions to lock down

**No code.** These choices constrain everything else.

| Decision | Options | Criterion |
|---|---|---|
| Language | Rust + libbpf-rs · C + libbpf · Go + cilium/ebpf | CO-RE mandatory; Rust if the correlator is large |
| Kernel floor | e.g. 5.15 (BTF, ringbuf) or 6.1 (`lock:contention_*`) | determines which fallbacks must be written |
| Clock | `CLOCK_MONOTONIC` **everywhere** | non-negotiable, see Phase 1 |
| Privileges | root · `CAP_BPF`+`CAP_PERFMON` · unprivileged degraded | changes half the backends |
| Scope | single machine (phases 1-8) · fleet (later) | multi-host adds inter-machine clock drift |
| Usage target | one-off session · continuous daemon · daemon with a flight recorder | sampling vs capture, online aggregation vs raw storage. The third is the second plus a trigger, and it is what catches a p99.9 that cannot be reproduced. Settled in the ninth wave: a session, with hot attach and the flight recorder inside it and no daemon (`DECISIONS.md`) |

**Output**: a filled-in `DECISIONS.md`. Every line settled, with the reason.

*Fifteenth wave: three of these lines were amended in `DECISIONS.md` after the
code answered them differently. The language is Rust with no libbpf: the
programs are assembled in Rust and relocated from the kernel's BTF at load.
There is no single kernel floor: each backend carries its own `kernel>=`, and
5.15 is served. And the privileges line now covers user-mode perf events at
`perf_event_paranoid` 2 (`DECISIONS.md`, "Settled in the fifteenth wave").*

---

## Phase 1 — Event model and request schema

The only genuinely expensive deliverable to get wrong. To be done on paper before
any code.

### Tasks

1. **`Event` struct**: `ts_ns`, `ts_err_ns`, `kind`, `keys`, `attrs`,
   `collector_id`, `seq`. Append-only, never mutated.
2. **Typed keys**: `task:tid`, `proc:pidns+pid`, `file:dev+ino`, `sock:cookie`,
   `blk:rq@open_ts`, `mem:[base,len)`, `gpu:fence_ctx+seqno`, `req:<id>`,
   `cgroup:id`. Only three match families: **equality**, **scoped equality**
   (pointer + open timestamp), **interval**.
3. **`Span` struct**: `id`, `parent_id`, `start/end`, `keys`, `rule_id`,
   `evidence` ∈ {`IDENTITY`, `KEY`, `WAKEUP`, `TEMPORAL`}.
   *Decided, not built: evidence lives on `join::Correlation` (one rule) and
   `join::Attribution` (one upstream state), and the tree is a decomposition.
   A span per matched event would be unbounded (`DECISIONS.md`, "Settled in
   the fifteenth wave", "No `Span` struct").*
4. **`Request` struct**: `source`, `target`, `mode`, `emit`, `keys`, `filter`,
   `budget`. A single object built by the CLI, the YAML file, or the API.
5. **`ResolvedRequest` struct**: the request plus the selected backend, pushed-down
   vs userspace filters, machine capabilities, degradations. This is the
   "lockfile".
6. **Clock policy**: `CLOCK_MONOTONIC` as the only domain. On the
   `perf_event_open` side, set `use_clockid` + `clockid` explicitly. On the eBPF
   side, **`bpf_ktime_get_ns()` only** — `bpf_ktime_get_boot_ns()` includes
   suspend time; mixing the two is a silent bug that destroys every join. Device
   domains (GPU, NIC): explicit calibration + `ts_err_ns`.

### Exit criteria

- [~] The five structs are written and documented.
      *Audit, fourteenth wave: Four of the five are written and documented:
      `Event`, the typed keys, `Request` and `ResolvedRequest`. There is no
      `Span` with id, parent_id and rule_id. Evidence lives on `Correlation` and
      `Attribution` in `src/join.rs`, and no decision records that change.*
      *Fifteenth wave: the decision is recorded (`DECISIONS.md`, "No `Span`
      struct"). Judged at the merge: left `[~]`. A decision is not a struct,
      and the criterion's "written and documented" is held by no test for any
      of the five; the four that exist round-trip under
      `event::tests::an_event_survives_a_round_trip_through_json_and_yaml`
      and `tests/cli.rs`'s dump tests. The box closes if the criterion is
      rewritten to the four and the decision, which is a roadmap edit, not a
      measurement.*
- [x] A unit test proves that a serialized/deserialized `Event` is identical.
      *Audit, fourteenth wave:
      `an_event_survives_a_round_trip_through_json_and_yaml` round-trips one
      sample with two key kinds, `Uint` and `Text` attrs, and a zero
      `ts_err_ns`. A non-negative `AttrValue::Int` does not survive: the
      untagged enum reads it back as `Uint`.*
      *Fifteenth wave, proven: `event::tests::an_event_survives_a_round_trip_through_json_and_yaml`
      now carries every `KeyValue` and `AttrValue` variant, `ts_err_ns` 1250,
      zero, negative, `i64::MIN`/`MAX` and `u64::MAX`, and compares by `Debug`
      as well as `==`; `a_non_negative_signed_integer_reads_back_as_what_was_written`
      holds the canonical integer (`DECISIONS.md`, "Settled in the fifteenth
      wave").*
- [x] A one-page note justifies each key: which boundary it crosses.
      *Audit, fourteenth wave: No note exists. Only some of the doc comments on
      `KeyKind` and `KeyValue` say what their key crosses (sock, blk, proc,
      iface, dev, cpu, node), and task, file, gpu and cgroup get no reason at
      all.*
      *Fifteenth wave: `docs/keys.md`, each key with the boundary it crosses,
      what it survives and what it does not, and where it is produced. Writing
      it found two things: `proc` has no producer, and `iface` is compared by
      its name alone, so `net.iface`'s `lo/drops` cannot join `net.qdisc`'s
      `lo`.* *Ticked at the merge: the criterion is a document, and the
      document exists, every producer named from a grep of `src/collect/**`
      and every comparison read from `join::identity`. The two findings are
      filed (`docs/open-findings.md`, stream docs).*
      *Sixteenth wave: both findings answered. `iface` is compared by the
      kernel's index (`join::tests::an_interface_is_one_key_by_its_index_across_a_rename_and_a_sub_series`,
      held to the kernel's own byte count by
      `join_engine::two_sources_of_one_interface_join_by_it_and_credit_what_lo_moved`);
      `proc` stays as vocabulary with no producer, refused by name in a join
      (`DECISIONS.md`, "Settled in the sixteenth wave"). `docs/keys.md`
      rewritten for both; the network namespace beside the index is open.*

---

## Phase 2 — Catalog, resolver, CLI skeleton

Still **no collector**. We build the machinery that will host them.

### Tasks

1. **Catalog loader**: parse the YAML entries (catalog schema §1).
2. **Machine introspection**: read `/sys/kernel/tracing/events`,
   `/sys/bus/event_source/devices`, kernel BTF, `perf_event_paranoid`, BPF
   capabilities, kernel version. Produces a `Capabilities`.
3. **Resolver**: `(source, mode, capabilities, scale) → backend`. Walks the
   backend chain, keeps the first viable one, records the degradations.
4. **Validator**: rejects invalid combinations **before any attachment**, with a
   message that says why (e.g. `mem.bandwidth --pid` → uncore is inherently
   system-wide).
5. **CLI**: `omn <family>.<metric> [options] [target]`, plus `omn list` and
   `omn --dump-request`. The CLI **builds a `Request`**, it executes nothing.
6. **`omn list`**: a catalog projection **filtered by actual availability**. Not a
   parallel list to maintain.

### Exit criteria

- [x] `omn list` shows different sources on two different machines.
      *Audit, fourteenth wave: Partly: the portability-matrix judges tie each
      cell's listing to a condition the cell sets up (paranoid 4, BTF hidden,
      kernel floor), so listings can only pass by differing. But no test
      directly compares two cells' listings, and the judges skip when no cell is
      on disk.*
      *Fifteenth wave, proven:
      `portability::two_cells_list_different_sources_for_the_condition_between_them`
      compares the listings of cells one condition apart (caps against root,
      nobtf, nocaps, notracefs, per kernel, and `here-caps` against
      `here-nocaps`), and passes on the 22 cells of the 2026-09-30 sweep: 45
      usable without BTF against 75 to 81 with it, 41 without the
      capabilities against 71 to 80. It skips loudly without cells.*
- [x] `omn --dump-request io.latency --pid 1` prints a complete `ResolvedRequest`.
      *Audit, fourteenth wave: Partly: the exact command is run in tests/cli.rs,
      but when it succeeds its output is never inspected, and the
      ResolvedRequest record has since moved to `--dump-resolved` (`--dump-
      request` now prints a session file). No test checks a complete resolved
      record for io.latency --pid.*
      *Fifteenth wave, proven:
      `tests/cli.rs::the_resolved_request_for_a_live_pid_is_complete_and_says_what_the_machine_says`
      dumps `io.latency --pid <a live sleep>`, deserializes it into
      `ResolvedRequest` and re-serializes it to the same document, and holds
      the machine it records to the machine asked another way (`uname`,
      `nproc`, clocksource, governor, SMT, THP, `perf_event_paranoid`, every
      vulnerability file) and the chain to the catalog's. The command is
      `--dump-resolved` and the pid a live one, not 1; without the
      capabilities it skips with the resolver's refusal.*
- [x] Three invalid combinations fail with three distinct, clear messages.
      *Audit, fourteenth wave: Proven: tests/cli.rs, tests/family_io.rs and
      src/validate.rs refuse more than three invalid combinations (uncore with
      --pid, a rate from an instantaneous gauge, a mode/emit mismatch, a results
      format for a dump...), each with its own message; the uncore refusal also
      checks that nothing is printed on stdout.*
- [~] No collector exists yet. **If collection code was written here, that is a
      sequencing error.**
      *Audit, fourteenth wave: Superseded: a sequencing rule, not a property of
      the code, and moot since Phase 3 added collectors. The history (33c73bf)
      shows the skeleton and the first collector landed in the same commit.*

---

## Phase 3 — First collector and transport plumbing

Two collectors only, chosen to exercise the entire plumbing.

### Tasks

1. **`sched.wakeup`** (`bpf:tp` sched_wakeup) — carries the wakeup chain, a
   dependency of every other family.
2. **`sched.offcpu`** (`bpf:tp` sched_switch + stacks) — exercises in-kernel
   aggregation.
3. **Transport**: `BPF_MAP_TYPE_RINGBUF`, userspace consumer, decoding into
   `Event`.
4. **Loss accounting**: a dropped-event counter, surfaced **all the way to the
   user-visible output**. A collection that loses 3 % without saying so produces
   wrong numbers that look right.
5. **Reordering**: ring buffers are per-CPU, events arrive unsorted. Bounded
   window + **watermark**: finalize nothing until every collector has passed T.
   This assumes a minimum pipeline latency.
   *Decided otherwise: no window and no watermark. Each collection is sorted
   when its window closes, and the join engine sorts what it merges, opening
   side first at an equal instant. A drain's lateness is unbounded, so any
   watermark would drop or misplace what came after it. What that costs a
   live stream is in `DECISIONS.md`, "Settled in the fifteenth wave", "No
   reorder window".*
6. **Symbolization**: frame pointers first. DWARF/ORC later (blazesym or libdw).
   Do not write your own DWARF unwinder.

### Exit criteria

- [x] `omn sched.offcpu --pid X --emit=hist` produces a correct histogram.
      *Audit, fourteenth wave: Proven by `scenario_sched::an_off_cpu_histogram_i
      s_of_blocks_and_finds_both_modes_of_a_bimodal_sleeper`: a sleeper with two
      modes lands in the two expected log2 buckets, checked against the thread's
      own voluntary-switch counts. The test launches the program rather than
      attaching by `--pid`.*
- [x] The loss rate is displayed, and it is zero under moderate load.
      *Audit, fourteenth wave: Partly: the loss is counted and shown (as a count
      next to the events seen, not a rate), and `no loss` is asserted on light
      sched workloads. No test holds zero loss under a defined moderate load,
      and the overflow test skips on a machine that never fills the ring.*
      *Fifteenth wave, proven:
      `scenario_sched::a_moderate_wakeup_rate_loses_nothing_and_says_so`
      defines the load by its rate (about 6 000 wakeups a second against the
      468 100 the ring drains, asserted under 2 %) and holds the loss line to
      0 dropped and the kept wakeups to the program's own count;
      `an_overflowing_wakeup_ring_says_what_it_could_not_hold` fills the ring
      by construction on every machine, in one run, against `FUTEX_WAKE`'s
      return values. The loss is still a count beside the events seen, which
      is the rate's numerator and denominator.*
- [x] A test with a program that sleeps N ms measures N ms ± 5 %.
      *Audit, fourteenth wave: Proven by `family_sched::a_program_that_sleeps_a_
      known_time_is_off_cpu_for_that_time`: a 400 ms `sleep` is reported as 400
      ms off cpu within 5 %, with no loss.*
- [~] Reordering is validated by injecting out-of-order events.
      *Audit, fourteenth wave: Partly: the join engine is shown to be order-
      independent on a reversed three-event input
      (`join::input_order_does_not_change_the_answer`). The runner sorts each
      collection after the fact, and there is no watermark to validate. No
      decision records that the sort replaces the watermark.*
      *Fifteenth wave: the decision is recorded (`DECISIONS.md`, "No reorder
      window"). What the criterion can still ask is that the runner's sort
      holds too: a collector's events handed over reversed, and the
      collection read back in timestamp order. Judged at the merge: left
      `[~]`, since no test injects out-of-order events into the runner.*

---

## Phase 4 — The vertical case (the real milestone)

**An HTTP request that reads a file and sends it back.** It crosses network RX,
scheduling, CPU, synchronization, VFS, page cache, block, device, network TX.

### Boundaries to implement

| # | Boundary | Upstream → downstream key | Mechanism | Evidence |
|---|---|---|---|---|
| 1 | NIC → softirq → TCP | skb → `sock:cookie` | `bpf_get_socket_cookie` | KEY |
| 2 | socket ready → thread woken | `sock:cookie` → `task:tid` | `sched_wakeup` | WAKEUP |
| 3 | request entry | `task:tid` → `req:id` | `bpf_task_storage` | IDENTITY |
| 4 | lock wait | `task:tid` → holder | `sched_switch` + `wakeup` | WAKEUP |
| 5 | `read()` → cache miss | fd → `file:dev+ino` | fd resolution at entry | KEY |
| 6 | FS → block submission | → `blk:rq@ts` | **re-read `task_storage` at `block_rq_issue`** | IDENTITY |
| 7 | submit → completion | `blk:rq@ts` | scoped equality + TTL | KEY |
| 8 | completion → wakeup | rq → `task:tid` | `sched_wakeup` from IRQ | WAKEUP |
| 9 | response | `req:id` → `sock:cookie` | `task_storage` | IDENTITY |
| 10 | TCP TX → NIC | `sock:cookie` → PHC | `SO_TIMESTAMPING` | KEY |

**Step 6 is the crux.** Do not join `bio → inode → file → fd → task` (fragile, 4
inferred joins). Read the `req:id` from the current task's storage at
`block_rq_issue`: a read is submitted in the reader's context. One propagated key
replaces four inferences.

### Deliverable

```
req:a3f1   total 412 ms
├── net_rx              0.4 ms   KEY
├── sched_wait          8.1 ms   WAKEUP
├── cpu_on             12.3 ms   IDENTITY
├── lock_wait          41.2 ms   WAKEUP     → holder tid=8821
├── block_io          347.0 ms   IDENTITY   → 3 rq, dev 259:0
└── net_tx              2.8 ms   KEY
```

### Exit criteria

- [x] The latency budget decomposes on a real server, not a mock. *A TCP
      server on loopback, `examples/vertical-one-request.yml`, ninth wave.*
      *Audit, fourteenth wave: `tests/vertical.rs` runs the loopback TCP server
      from `examples/vertical/server.c` under `examples/vertical-one-
      request.yml` and checks every row against the server's and the client's
      own clocks.*
- [x] Every line carries its evidence level. *`IDENTITY` for every row read by
      the request id the serving thread carried.*
      *Audit, fourteenth wave:
      `the_network_rows_are_read_by_the_socket_the_server_accepted` holds the
      four thread rows to `IDENTITY` and `net_rx`/`net_tx` to `KEY`, the socket
      cookie being the server's own `SO_COOKIE`. The `block-io` table's rows are
      not checked yet, and a row's evidence is optional in the engine.*
      *Fifteenth wave, proven:
      `vertical::the_block_io_rows_carry_the_identity_the_block_requests_read_back`
      holds the `block-io` rows and whole to `IDENTITY`, the device row's
      readings equal to the server's own `done_reads`, and
      `join::tests::a_row_without_an_evidence_level_is_never_built_and_never_read`
      makes a row without its level impossible to build or read (only a
      bracket whole, the window or the life, goes without).*
- [x] The sum of the segments ≈ the client-side measured latency (gap < 10 %).
      *+0.11 % to +0.42 % over six runs, one of them under load; positive every
      time, as it has to be — the loopback and the parse are the client's.*
      *Audit, fourteenth wave:
      `the_segments_sum_to_the_latency_the_client_measured` holds each of five
      requests within 10 % of the client's own clock; the slow reader is the
      widest, at about +3 %.*
- [~] **Ground truth**: a synthetic server with known injected delays (`tc netem`,
      `dm-delay`, `usleep` under a lock) is decomposed correctly. Without that
      test, nothing proves the correlation is right rather than merely plausible.
      *`usleep` under a lock and a cold read, each landing in its own row; since
      the tenth wave, a client's 30 ms mid-request pause in `net_rx` and a slow
      reader's window in `net_tx`, injected from the client, which is what `tc
      netem` was for and needs no root. `dm-delay` is the one injection left.*
      *Sixteenth wave: `dm-delay` is injected and held, but not on the server.
      `dm_delay::a_device_delayed_from_outside_is_measured_with_the_delay_above_the_device`
      sets 20 ms on every I/O of a loop device and holds `io.syscall` and
      `io.latency` to it: 482.0 ms above the device for 24 I/Os, 480 ms
      configured (floor 456, one tick less per I/O), 0.54 ms on the loop
      device. That is two
      sources compared by the test, on a program issuing direct I/O; no
      `decompose:` of the vertical server puts the delay in its row yet. Left
      `[~]`: it closes when `examples/vertical-one-request.yml`'s block-io
      table reads a file on the delayed device and its row carries the delay.*

---

## Phase 5 — Generalizing the join engine

Now, and not before: we have three real join types to generalize from.

### Tasks

1. **Declarative rules**: `{from, to, via, window, match}` where `match` ∈
   {`eq`, `scoped_eq`, `interval`, `wakeup`}. The engine knows nothing about `bio`
   or `malloc`, it applies rules.
2. **Join placement**: kernel (eBPF map) / agent (stream) / query (storage).
   **Decided per rule, not globally.** The model must allow moving a join from one
   tier to another without rewriting the collector.
3. **Open-state lifecycle**: TTL, bounded map size, **eviction counters
   surfaced**. Delete on completion, not only on expiry.
4. **Scoped equality**: `(pointer, open_ts)` everywhere a kernel pointer is used as
   a key. Pointers get recycled.

### Exit criteria

- [~] Adding a boundary means adding a rule, zero lines of engine.
      *Audit, fourteenth wave: Partly: the engine names no source, and the
      crossings (`tests/cross_memory.rs`, `tests/cross_io.rs`) add boundaries
      through session `joins:` only. No test holds the property, though, and a
      rule cannot choose its tier (kernel, agent or query), so a kernel-side
      boundary is still collector code.*
      *Fifteenth wave: held for the agent tier by
      `join_engine::a_new_boundary_is_a_rule_in_a_session_and_no_line_of_engine`.
      Left `[~]`: a rule still cannot choose the kernel or query tier.*
      *Sixteenth wave: unchanged. A kernel-tier rule would be a program of
      `src/bpf/` per match kind, and the query tier needs a command over
      stored runs, which `omn` does not have (`docs/fixes/w16-engine.md`).*
      *Eighteenth wave: a rule chooses its tier (`tier: agent`, `query` or
      `kernel`). The query tier applies the same engine to a stored run's
      `raw` events (`join::apply_stored`), held equal to the agent tier field
      for field on a real run
      (`join_engine::a_rule_moved_to_the_query_tier_answers_from_the_stored_run_what_the_agent_did`);
      the kernel tier is refused with the reason, since no match kind has a
      program (`docs/fixes/w18-engine.md`). No command applies a rule to a
      stored run yet. Left `[~]` for the kernel tier only.*
- [x] A load test proves there is no open-state leak over 1 h.
      *Audit, fourteenth wave: Not proven: no join-engine load test of any
      length exists. The open-state balance (`states_balance` in `src/join.rs`)
      is checked on hand-built events of a few states only, and rules are
      applied after the run rather than streamed.*
      *Fifteenth wave, partly:
      `join_engine::a_join_under_load_ends_every_state_it_opened_and_holds_no_more_than_its_map`
      (1.5 M events, in the suite) holds every count to the generator's, the
      balance, peak open states equal to the capacity, peak heap linear, and
      nothing live afterwards. The hour is
      `join_engine::an_hour_of_joins_leaks_no_state` (`#[ignore]`,
      `OMN_JOIN_LOAD_SECS`), run for 300 s in debug (24 rounds of two million
      events, every round's peak equal, nothing live), not for 3600 s. Ticks
      when `cargo test --release --test join_engine -- --ignored an_hour_of_joins`
      has run to the end on a quiet machine.*
      *Sixteenth wave, proven: `join_engine::an_hour_of_joins_leaks_no_state`
      ran its full 3600 s in release on a quiet machine (`target/join-hour.log`,
      2026-10-02): 1500 rounds of 2 001 347 events (790 600 states each),
      every state the generator opened ended, nothing live at the end, and
      every round's peak 0.7 MiB beyond its stream, the same in all 1500
      rounds. The engine is also handed a run's collections apart now
      (`join::apply_merging`, called by `assemble`), so a real run's join
      holds no index of an ordered collection.*
- [x] Evictions appear in the user-visible output.
      *Audit, fourteenth wave: Partly: the engine counts expired and overflowed
      states on hand-built inputs (`src/join.rs` tests), and
      `encode::an_eviction_is_never_quietly_absent_from_the_output` renders
      `EVICTED`. No end-to-end `omn` run with a join that really evicts checks
      that the count reaches the output.*
      *Fifteenth wave, proven:
      `join_engine::a_join_that_evicts_says_how_many_in_results_json_and_in_the_text`
      runs `omn` on a session whose join evicts, and finds the count in
      `results.json` and in the text.*

---

## Phase 6 — Emit, aggregation, budget

### Tasks

1. **Emits**: `raw`, `hist` (log2 and linear), `rate`, `topk`, `stack`, `timeline`.
2. **In-kernel aggregation**: histograms and top-k computed inside the eBPF
   program. That is what makes the overhead viable — two million events become a
   few hundred bytes.
3. **`--budget N%`**: measure the collector's actual cost (consumer CPU, loss
   rate), degrade automatically (`trace` → in-kernel `hist` → `sample`), **and say
   what was degraded**.
4. **Hot attach/detach**, without restarting the agent or breaking in-flight
   joins. A constraint to set now: it conditions the escalation in phase 10.

### Exit criteria

- [~] `io.latency --emit=hist` at 2 M IOPS stays under 1 % overhead.
      *Audit, fourteenth wave: Not proven. No test measures `io.latency` under
      an IOPS load. `src/bpf/io.rs` streams every completion instead of building
      a kernel-side histogram, so the join keeps `blk:rq@open_ts`. The only
      measured cost is `docs/bench.md`'s ordinary workload: 3.70 % tool cpu and
      +0.36 % dilation.*
      *Fifteenth wave, partly: the histogram is built in the kernel when it is
      the whole request (`scenario_io::a_histogram_built_in_the_kernel_is_the_one_the_streamed_requests_make`,
      held to the program's own count of its reads, per path, before the two
      paths are compared). The overhead test is written,
      `scenario_io::the_in_kernel_histogram_costs_a_two_million_iops_load_under_one_percent`
      (`#[ignore]`), and not run: it needs `null_blk`, a module load
      (`STATE.md`, "Needs privilege"). The disk here reaches about 12 000
      reads a second.*
      *Sixteenth wave: unchanged, still waiting on `null_blk`, which this
      wave did not load (`STATE.md`, "Needs privilege", item 1).*
      *Seventeenth wave: run once by the coordinator before the wave, on
      `null_blk` at 3.16 M IOPS: 23.57 %, 781 ns a request (bare 3 158 519
      IOPS, 2 413 950 under the histogram). The histogram now has its own
      program pair (`src/bpf/io.rs`, `docs/scenarios/io.md` defect 19), and
      the test asserts the histogram counted fio's reads within 1 % before it
      trusts the percentage. The new cost is not measured: the run is the
      coordinator's, alone (`STATE.md`, "Needs privilege"). Whether 1 % is
      the right criterion is open (`DECISIONS.md`).*
      *Eighteenth wave: run once after the seventeenth wave's merge, 12.03 %
      (348 ns a request), and with `bpf_stats_enabled` the programs read 0
      runs: they were closed at the stop, before the run read their account.
      They are now detached and kept until the close
      (`docs/fixes/w18-io.md`), so the next run says how much of the 348 ns
      is the programs'. Not run since; the decision waits on it.*
      *After the eighteenth wave's merge (coordinator, 2026-10-04): 11.54 %,
      332 ns a request (bare 3 143 826 IOPS, 2 781 156 under the histogram).
      With `bpf_stats_enabled` 1, 15.31 %, 457 ns, and the programs ran
      53 622 158 times (twice the requests) for 149.3 ns a run, 298.5 ns a
      request: most of the cost is the programs' own run time, so `tp_btf`
      for the two device reads is the next cut. Not met; whether 1 % stays the
      criterion is still open (`DECISIONS.md`).*
- [x] Exceeding the budget produces an **announced** degradation, not a loss.
      *The runner checks every 100 ms and, over the ceiling, moves the
      collection onto the next cheaper backend of its chain that this machine
      serves (`Ladder` in `src/main.rs`), after draining the outgoing
      collector's events into the run; the note says when and which way it
      moved (`degrade` in `src/collect/mod.rs`). With nothing cheaper, the run
      stops and names the ceiling. `tests/budget.rs`,
      `a_run_over_its_ceiling_moves_onto_a_cheaper_backend_and_says_when`.*
      *Audit, fourteenth wave: Partly proven. `tests/budget.rs` holds the
      announcement: the note says when and which way the run moved, and a run
      with nowhere cheaper to go names the ceiling. The drain in `degrade` has
      no test that counts events across the move. The only move tested is
      between two polled `mem.rss` backends, not trace to hist to sample.*
      *Fifteenth wave: the drain across a move counted against a task's own
      count, and the seam announced
      (`budget::a_budget_s_move_between_backends_drops_nothing_and_says_where_both_counted`:
      `sched.switches` moved from `pmu:sw` onto `bpf:raw_tp`, nothing dropped,
      nothing counted twice outside the announced overlap). The mode ladder,
      trace to hist to sample, is not built: it waits on the open decision
      "what one series made of two modes means to an emit" (`DECISIONS.md`).
      Left `[~]`.*
      *Sixteenth wave, proven: the decision is settled (`DECISIONS.md`,
      "Settled in the sixteenth wave") and the ladder steps backends of one
      mode, then `trace` -> `sample` on a collector that weighs its sampled
      events, which `mem.faults` on `pmu:sw` does since the merge.
      `budget::a_traced_series_over_its_ceiling_counts_what_the_task_did_and_moves_only_onto_weighed_samples`
      holds the moved series to the task's own `getrusage` count of its
      faults: 1 707 073 counted, 0 lost, against 1 706 035 to 1 708 083 inside
      the window, the move announced with its overlap. A request whose emits
      cannot survive the move is refused before the run (exit 2), and a rung
      it cannot take is named as passed over
      (`budget::a_budget_takes_no_mode_rung_whose_sampled_events_would_not_be_weighed`).
      The budget weighs the target in nanoseconds
      (`budget::a_target_that_burns_under_a_tick_per_check_is_weighed_and_the_budget_acts_on_it`).
      Task 3's in-kernel `hist` is taken at the start when the emits allow
      it, never stepped onto, by that decision.*
- [x] A collector can be added and removed mid-session. *A request's `window`,
      tenth wave: one phase of a program counted within +0.17 %, and nothing of
      the other. The join engine's view of a detached source is filed.*
      *Audit, fourteenth wave: Proven by `tests/runner.rs`. Counters attached
      and detached mid-run, one per phase of a program with a known instruction
      count, count their own phase within +0.5 % and nothing of the other, and
      the windows sum to the whole. An eBPF probe attached at 1 s sees exactly
      the second phase's 26 wakeups. Joins across a detach remain filed.*

---

## Phase 7 — Declarative configuration

### Tasks

1. **Session file** (YAML): sources, filters, joins, emit, duration. Builds **the
   same `Request` object** as the CLI.
2. **`omn --dump-request`** prints the equivalent config: prototype on the CLI,
   freeze into a file. No divergence possible between the two surfaces.
3. **Resolved file emitted next to the results**: backend selected per source,
   placement of each filter, captured environment (kernel, BTF, microarchitecture,
   mitigations, clocksource, governor, SMT, THP mode, NUMA, virtualization, steal
   time, privileges). This is what makes two measurements comparable across
   machines.
4. **Variables and includes**, to replay a session across N machines or N
   workloads. A handful of variables is enough — do not reinvent Helm.

### Exit criteria

- [x] A CLI session → file → replay produces an equivalent result.
      *Audit, fourteenth wave: Partly: the request round trip is proven (the CLI
      dump read back through `-f` is byte-identical, in YAML, JSON and TOML:
      `tests/cli.rs`). But no test runs the CLI measurement and its file replay
      against a known workload and compares the results.*
      *Fifteenth wave, proven:
      `tests/cli.rs::a_session_dumped_from_the_command_line_measures_what_the_command_line_measured`:
      a C program making exactly 1000 allocations of 77777 bytes, measured
      from the command line and from the session file that command line
      dumps; both match the program's own numbers, and the replay's series,
      filtered counts and events seen equal the command line's exactly.*
- [x] The resolved file is enough to explain a gap between two machines.
      *Audit, fourteenth wave: Not proven: tests check only that fields such as
      governor, SMT, clocksource and kernel release are present, and only the
      GPU record is checked against sysfs. Microarchitecture, NUMA,
      virtualization and steal time are still in `not_captured`. No test
      explains a gap between two machines from their two resolved files.*
      *Fifteenth wave, partly:
      `portability::every_gap_between_two_cells_is_explained_by_their_resolved_records`
      credits each gap to the cause the refusing probe names, only when the
      field standing for it differs the way that refuses (judged by
      `a_gap_is_credited_to_the_cause_the_refusal_names_not_to_any_field_that_differs`).
      On the 22 cells on disk: 3182 explained (`kernel_btf` 1716, `tracefs`
      991, `kernel_release` 255, `gpus` 200, `perf_event_paranoid` 20), 83 the
      build's, and 2754 unrecorded, which only the privileges and the
      virtualization could explain. The record carries both now
      (`Environment.privileges`, `.virtualization`), but the cells were swept
      before it did. Ticks when a sweep with this binary leaves no gap
      unrecorded (`STATE.md`, "What remains", item 1).*
      *Sixteenth wave, proven: the 22 cells were swept again with one binary
      (`cd394a8a8550c199`, 2026-10-02), each recording its privileges, its
      virtualization and `cell.build`, and
      `portability::every_gap_between_two_cells_is_explained_by_their_resolved_records`
      passes: among the fresh cells every gap is explained by a recorded
      field that differs the way that refuses, none is the build's and none
      waits on a missing field. Two judge defects were fixed first (a refusal
      with two causes, `Cause::KernelBelowOr`, and an unwritten probe counted
      as the build's only where the cells answered it differently). The five
      `7.2.6-*` cells on disk predate the host's 7.2.7 and the fields; they
      alone account for the judge's 1316 "not explained yet", and deleting
      them leaves none.*

---

## Phase 8 — Self-benchmark

The initial PoC, turned against the tool itself. This is where `--backend force:X`
finally earns its place.

### Tasks

1. **Harness**: reference workloads (launch-bound, compute-bound, memory-bound,
   syscall-heavy, IO-heavy, contended multithread), warmup, ≥ 20 repetitions,
   round-robin between methods, median + IQR.
2. **Variance gate**: baseline without any tracer. **If a workload's median cannot
   resolve a 1 % difference, the bench cannot resolve a 1-3 % overhead on it** --
   give it the repetitions its spread needs, or name it and leave it out; and if
   the rest of the machine is busy, fix the environment before measuring
   anything. *Written first as "CV > 1 %", which held a median of twenty to the
   spread of one run: it refused a quiet machine every time and passed one with
   a steady load on every cpu. `docs/bench.md` has both measurements.*
3. **Environment control**: `performance` governor, locked clocks, frozen THP,
   affinity, invalidation of any throttled run (`power.*` collected
   unconditionally).
   - **And the caches, which this list was missing.** A run leaves the last
     level cache and the NUMA node's memory in whatever state it left them, and
     the next run inherits it: `docs/scenarios/mem.md` already records that a
     shuffled walk over 64 MiB and one over 4 KiB differ by two orders of
     magnitude, which is the same effect seen on purpose. So the protocol
     evicts the LLC and sweeps each NUMA domain between repetitions, the way it
     already drops the page cache — a cold cache and a warm one are two
     measurements, and a harness that does not choose between them is measuring
     the order its runs happened to go in.
   - **Prefetcher state is captured, not controlled.** Disabling the hardware
     prefetchers is a real lever for isolating a memory effect, and it is also a
     modification of the target — boundary 5. So the state is read and written
     into the resolved file beside the governor and the THP mode, and whoever
     wants it off turns it off themselves and gets a resolved file that says so.
     Two runs on the same machine with different prefetcher settings are not
     comparable, and nothing today would say why.
4. **Backend comparison**: for each source, force every backend in the chain and
   measure cost, effective resolution, loss rate. **Validates that the resolver
   chooses correctly.**
5. **Effective resolution**: are two events X µs apart distinguished? Sweep X from
   0.5 to 50 µs per backend.
6. **Observer effect**: does the collector introduce serialization? Compare an
   asynchronous sequence with and without it.

### Exit criteria

- [x] A cost/resolution/loss table measured for every (source, backend). *94
      pairs, after the tenth wave, `docs/bench.md`; the heap sources from a
      rerun.*
      *Audit, fourteenth wave: Partly: the table comes from one ignored run in
      the tenth wave, 94 pairs in `docs/bench.md`. Only three pairs are checked
      against known counts, and resolution only for three timestamping backends.
      The heap rows and every backend added since still need a rerun.*
      *Fifteenth wave: the table gains a `band` column, each row held to its
      declared band on the larger of its share of the target's cpu and its
      dilation; a row missing a column fails the table, an over row is
      flagged (`bench_harness::a_row_is_held_to_its_band_on_the_larger_of_its_share_and_its_dilation`,
      `a_row_with_a_column_missing_is_refused_rather_than_judged`). The
      table's own run (`tests/bench_backends.rs`, `#[ignore]`) was not rerun.
      Left `[~]`.*
      *Sixteenth wave: `harness::table_failures` now fails the whole table
      when it has no row, when a planned pair is neither a row nor excluded
      by the gate, or when a row was served by another backend than the one
      it forced (`bench_harness::a_wrong_backends_table_fails_and_a_row_over_its_band_alone_does_not`,
      in the suite). The table was still not rerun: `scripts/bench.sh`
      measures `target/release/omn`, whose capabilities need root (`STATE.md`,
      "Needs privilege"). Left `[~]`.*
      *Seventeenth wave: the table ran before the wave (2026-10-03,
      `scripts/bench.sh`, 68 minutes, its test green, documents under
      `target/bench/backends-1791030430/`). 46 (source, backend) pairs are
      flagged over their declared band: worst `io.fsync` through
      `bpf:fentry` at 142 %, `net.tcp.connect` 39 %, `gpu.freq` through
      `nvml` 14.5 %, `power.cstate` 7.8 %, `cpu.profile` through `pmu:sw`
      4.9 %. The bands are not yet corrected to it, and the table is not in
      `docs/bench.md`. Left `[~]` until they are.*
      *Eighteenth wave, done: all 95 rows of that table are recorded in
      `docs/bench.md`, 43 `cost:` lines corrected to the band they measured
      and two standing with their reason (a fixed `bpf:fentry` attach), and
      `tests/bench_bands.rs` fails when a band and its row disagree or the
      record loses a row (`docs/fixes/w18-bench.md`). A table re-measured
      after the wave's fixes is the coordinator's next bench run.*
- [x] The resolver's choices are confirmed by measurement, or corrected.
      *Confirmed for the seven chains with two served backends. The table's
      correction is to the cost bands: an in-kernel collector's cost is in the
      target's dilation, not in the tool's cpu.*
      *Audit, fourteenth wave: Partly: the verdict rule is unit-tested on
      synthetic rows, and the seven confirmations come from one recorded run.
      The cost-band correction is recorded in `DECISIONS.md`, but no test holds
      the catalog's bands to a measured dilation.*
      *Fifteenth wave: the band column above is that test, once the bench's
      table is rerun on a quiet machine. Left `[~]`.*
      *Sixteenth wave: unchanged; the same rerun.*
      *Eighteenth wave, done: the table reran before the seventeenth wave and
      the catalog's bands are corrected to it, an in-kernel collector's cost
      read as the larger of the tool's share and the target's dilation, and
      `tests/bench_bands.rs` holds every band to its row, including that an
      `equivalent` backend never declares less than what it stands in for.*
- [~] The agent's own cost (CPU, RSS) is measured separately from the dilation of
      the observed process. *Tenth wave: collector cpu from `cost.json`, peak RSS
      from `/proc`, dilation from the workload's own clock.*
      *Audit, fourteenth wave: Partly: the harness measures the three columns
      from three independent sources (`cost.json`, VmHWM in `/proc`, the
      workload's own clock), but only in an ignored run that asserts nothing
      about them. The separation is shown in `docs/bench.md`, not in a test.*
      *Fifteenth wave: the agent's own cpu is now whole in `cost.json`
      (before, in and after the window), held to the kernel's `wait4` account
      of `omn` by `budget::what_a_profile_with_stacks_says_it_cost_adds_up_to_what_the_kernel_counted`,
      while the percentage stays over the window. The separation from the
      dilation is still asserted only in the bench's ignored run. Left `[~]`.*
      *Sixteenth wave: the `cost:` line now names the cpu spent before the
      window when it outweighs the window's, held to `cost.json` and to
      `wait4` by the same budget test. The separation from the dilation is
      unchanged. Left `[~]`.*

---

## Phase 9 — Broadening

Only now do we follow the catalog breadth-wise. Every added source is incremental
because the skeleton exists.

Suggested order: rest of `cpu` and `mem` → extended `io` and low-level `net` →
`proc` → advanced `mem` (bandwidth, c2c, wss) → `gpu` → `runtime` → `cpu.flow`.

`power` is not in that list because it was meant to be collected
unconditionally from phase 8 on, as a guardrail. **That is not what exists.**
The bench does not run `omn power.*` beside a measurement. Its harness reads
the kernel's `thermal_throttle` counters around every repetition and drops a
run they moved in (`examples/bench/harness.rs`, `throttle_count`). On an AMD
part, which publishes none of those counters, it says a throttled run cannot
be told from one that was not (`docs/bench.md`, "Environment control").
The sixteenth wave settled it (`DECISIONS.md`, "Settled in the sixteenth
wave"): **`power.throttle`'s counters, read directly by the harness, on
purpose.** The guardrail wraps the bare baseline runs too, which an `omn`
beside them would turn into traced runs, and a guardrail measured by the tool
under test would pass the tool's own defect in that source (`docs/bench.md`).

**Criterion per added source**: catalog entry + backend + fallback chain + ground
truth test + automatic appearance in `omn list`.

---

## Phase 10 — Intelligence layer

Above the engine, not inside it. It calls Omniscient like any other user.

The methods are the deliverable, not a menu. Each one names the sources it needs
and each is only worth building once they collect, so the order below is the
order in which their inputs land.

0. **The criterion comes first, and it is a parameter rather than a footnote.**
   "What is this workload limited by" has no answer until someone says *limited
   for what* — median latency, the tail, throughput, energy and cost do not name
   the same culprit, and improving one routinely degrades another. So a triage
   request carries its objective the way a measurement request carries its emit,
   and the answer names the objective it answered for. A layer that picks one
   silently is a layer that reports a confident wrong suspect, which is the
   failure this project is built against, one level up.
1. **USE triage** (Utilization / Saturation / Errors): a broad, cheap pass that
   answers "what is this workload limited by, for this objective?".
2. **TSA** (thread state analysis): where a thread's wall clock went, split
   across on-cpu, runnable-and-waiting, and blocked-on-what. **This one is
   ready first**, because its three inputs already collect and are checked --
   `cpu.profile`, `sched.runqlat` and `sched.offcpu` with its blocking stack --
   and the `wakeup` join already says what released a block. It is also the
   method that answers the question people actually arrive with, which USE does
   not: *why is this slow*, rather than *what is saturated*.
3. **Automatic escalation**: once the suspect is identified, enable that layer's
   detailed collectors and turn off the rest. The budget holds because we only pay
   dearly for one layer at a time. Depends on hot attach (phase 6).
4. **Question-oriented modes**: `omn latency --pid X`, `omn saturate`.
5. **Differential flame graphs**, which need nothing new: two profiles and a
   subtraction, and the encoders for both already exist.

### What this layer must not get wrong

Two arithmetics that look like presentation and are not:

- **Coordinated omission.** A `hist` built from a source that missed readings is
  biased, and biased *towards the answer the reader wants*: the readings a
  polled or sampled source loses are the ones taken while the machine was
  stalled, which are the slow ones. The collectors already count what they
  missed -- `late_ticks`, `throttled`, `dropped` -- and nothing anywhere says
  the distribution built on top of them therefore under-reports its own tail.
  This belongs to the emit layer, not to this phase, and it is recorded as
  finding 37 in `docs/open-findings.md`; it is named here because a triage layer
  reading a percentile is exactly who would be misled by one.
- **Little's law and the queueing identities** are how saturation is read from
  what is already collected -- `io.inflight` against `io.iops`, run queue length
  against `sched.runqlat`. They are worth stating as the arithmetic of the USE
  pass rather than leaving each source to imply them.

---

## Phase 11 — A model reads it

Last, and the ordering is the whole point. Omniscient already produces what a
language model needs and a human does not: typed events, attributions carrying
an evidence level, and a resolved file describing the machine, the backend and
every degradation. Handed that, a model can say what a run means without
inventing the parts it cannot see. Handed rendered text, it invents.

So the work is an MCP server exposing what already exists as resources — the
resolved file, the results document, the catalog — rather than a report
generator. Nothing new is measured and nothing new is modelled; this phase is a
second surface on the same objects, the way the session file is a second surface
on the same `Request`.

**It comes after phase 10 and not before.** A model summarising a correlator
nobody can yet interpret produces conclusions that are confident and wrong, and
the evidence ladder is exactly the thing it would flatten: `IDENTITY` and
`TEMPORAL` read alike in prose. Until the tool itself can say what a run was
limited by and for which objective, a model reading it has nothing to be right
about.

Two constraints that fall out of the rest of this file:

- **It measures only what its operator allowed, and never changes the
  machine.** Boundary 5 applies to this surface too. `omn-mcp` reads by
  default: the catalog, the runs a directory holds, and the sweep the `omn`
  binary answers. It runs a session only when the person who started it
  passed `--allow-run DURATION`, for at most that duration, with a budget
  required and output kept under the server's directory. It runs a session
  that launches a program only when that session is a file in the directory
  the person passed with `--allow-launch DIR`, which needs `--allow-run`
  beside it. That file is run as written, program, arguments, environment and
  working directory included, and nothing in a tool call changes it. A model
  chooses which approved session runs and when, never what runs (`DECISIONS.md`,
  the twelfth and thirteenth waves; `docs/mcp.md`).
- **Every number it is handed carries what it is missing.** Finding 37's
  `Shortfall` exists for a reader taking a p99 off a lossy run, and a model is
  that reader with less judgement, not more.

---

## Phase 12 — Programs nobody wrote for it

Every ground truth so far is a workload written for the test: a `usleep` of
known length, a known number of I/Os, `calltree.c` with its shares fixed by
construction. That is what made each collector checkable, and it is also why
none of the methods above has yet met a program whose behaviour nobody
designed. A real program brings what the fixtures leave out -- a thread pool,
frameless libc leaves, a stripped distribution binary, an interpreter's frames,
I/O and CPU interleaved in one request -- and a method that is right on its
fixtures and wrong there is the confident wrong suspect this project is built
against.

So this phase measures nothing new and adds no source. It takes programs that
were not written for Omniscient and holds the answer to *"why is this slow, for
this objective"* against a cause known independently of the tool.

**Where the cause comes from.** A real program has no share fixed by
construction, so the known cause is one of three, in this order of preference:

1. **The program's own account** of where its time went: `sqlite3`'s `.stats`
   and `EXPLAIN QUERY PLAN`, CPython's `cProfile`, `gcc -ftime-report`,
   `getrusage` and the kernel's `schedstat` for the process.
2. **A cause injected from outside the program**, never into it: an input that
   defeats an index, a file on a `dm-delay` device, a second process holding the
   lock the program waits on, a working set larger than the page cache. The test
   harness injects it; the tool never does (boundary 5).
3. **Two builds of the same program** that differ by one known change, where
   the difference is the cause.

### Tasks

1. **A case per program**, under `cases/<program>/`: the program and how it was
   obtained (distribution package, build flags), the sessions, the injected
   cause, the reference's own account, and the verdict expected for each
   objective. A case is data, like a catalog entry: adding a program means
   adding a directory, not code.
2. **The question asked both ways**: `omn triage` as a person would ask it, and
   `omn-mcp` as an agent would. The two answers name the same suspect.
3. **Each verdict says how it knows**: which method answered (USE, TSA, a
   decomposition, an escalation), the evidence level of each join it relied on,
   the unwind mode and where each name came from, and what it could not see.
4. **What a case finds wrong is fixed where it lives.** A case does not carry a
   workaround: a wrong suspect is a finding against the method, the collector or
   the unwinder that produced it, filed and fixed in the owning file.

Programs to start from, each available on a stock distribution: `sqlite3` (a
stripped distribution binary: names from debuginfod), CPython running a pure
Python loop and an I/O-bound script (interpreted frames the tool cannot read
yet, so the case checks that it says so), `loganalyzer.c` (parse, aggregate,
rank, report over 1.5 M lines), a compiler on a known translation unit, a
small HTTP server under a load generator, and a build (`make -j` on a real
project), which is many short processes rather than one.

### Exit criteria

*Seventeenth wave: six cases asked both ways by `tests/cases.rs`, each cause
through `omn latency` and through `omn-mcp` (`docs/cases.md`): `sqlite3-stripped`,
`sqlite3-durability`, `cpython-interpreter`, `cpython-perf-map`,
`http-server-stdlib`, `make-sqlite`, and `gcc-shell` held back under
`cases/held/` until the triage follows a wait for a child. What fails is
listed under each case's `open_findings` and filed where it lives
(`docs/open-findings.md`, the seventeenth wave's sections).*
*Eighteenth wave: seven cases, `gcc-shell` back in `cases/gcc-shell/` now
that the triage follows a wait for a child; both ways read the task the
triage follows, and two ways run with different privileges are not compared
(`docs/fixes/w18-cases.md`).*

- [~] Six programs, none written for this project, each with at least two known
      causes, one on the CPU and one off it, and for every case the triage's
      first suspect is the known cause, for the objective asked.
      *Seventeenth wave, partly: six cases over four programs (`sqlite3`
      twice, CPython twice, `http.server`, `make`; gcc's case held back), each
      with a cause on the cpu and one off it. The first suspect is the known cause, both ways, for
      `sqlite3-stripped`, `sqlite3-durability` and both CPython cases. It is
      not for `http-server-stdlib`'s listing, `make-sqlite` and the held
      `gcc-shell`: the triage decomposes the launched program's main thread,
      which waits in `select` or `wait4` while another thread or a child does
      the work ("a program is its main thread"; `DECISIONS.md`, "Open").
      `http-server-stdlib`'s cold file skips here with its numbers (the NVMe
      is faster than the server's copy).*
      *Eighteenth wave, partly: seven cases over five programs, `gcc-shell`
      back. The first suspect is the known cause, both ways, for every cause
      asked here, `make-sqlite`'s one-job cause in four runs of five (the
      triage follows one `cc1` of a job that is a chain of processes, filed);
      the server's listing is followed into its serving thread, and the
      compiles into `cc1`. Left `[~]` for `make-sqlite`.*
- [~] Every number a verdict cites agrees with the reference's own account,
      within that account's resolution.
      *Seventeenth wave, partly: within the stated slack for `sqlite3-stripped`
      (the on-cpu row 1 % over `.timer`'s cpu, the blocked row 29 ms over the
      holder's clock) and the CPython cases (2 % over `process_time`, 7 ms
      off the writer's clock). Not for `sqlite3-durability`'s `blocked` and
      `runnable` rows (2 to 3.5 % under the kernel's `schedstat`, runnable
      twice the run delay), `http-server-stdlib`'s slow client (the main
      thread's `select`, a second longer than the serving thread's wait), and
      `make-sqlite`.*
      *Eighteenth wave, partly: `sqlite3-durability`'s rows within 7 to 20 ms
      of the kernel's over 500 commits (the sched sources read the kernel's
      own wait); still not for `make-sqlite` and `gcc-shell`'s contended
      compile (one `cc1` of the job's chain), nor the slow client (a thread
      blocked beside the main thread is not followed). Filed.*
- [~] Where the cause is out of the tool's sight (interpreted or JIT frames, a
      closed GPU), the verdict says so by name. No case ends with a confident
      wrong suspect.
      *Seventeenth wave, not met: without a perf map the verdict presents an
      interpreter C function (`__tls_get_addr`) as the hottest; with one, the
      Python function is on 99 % of the stacks and never a leaf, so it is not
      the one named; and the main-thread cases end with a confident wrong
      suspect (`Limit::Sleep`'s "a timer, a pipe or a socket" for a wait for
      a child or a polled lock). Each is filed.*
      *Eighteenth wave, partly: a flat interpreter profile is said as one and
      names no suspect, a sample under a perf map is charged to its innermost
      perf-map frame (`py::score`, both ways), and the sleep sentence names a
      polled lock and a wait for a child. Left: the slow client's verdict
      names the main thread's `select` where the serving thread's send is the
      request's, and the tool's own eBPF frames (`bpf_trace_run2`) can be
      ranked as the hottest function. Filed.*
- [x] One case names a stripped distribution binary's functions from
      debuginfod, and says where each name came from.
      *Seventeenth wave, the agent's way: `sqlite3-stripped` through
      `omn-mcp` names `sqlite3VdbeExec` from debuginfod with "names came from
      -- sqlite3: debuginfod, ... build id 88b37b84..."; each frame says its
      source (`Frame::named_by`). The person's way cannot: `omn latency` takes
      no `--symbols`, the verdict drops the names note, and `sched.offcpu`'s
      stacks are not asked through debuginfod (all filed).*
      *Eighteenth wave, done: the person's way too. `omn latency --symbols
      debuginfod` names `sqlite3VdbeExec.lto_priv.0` and the block's
      `sqliteDefaultBusyCallback` (`sched.offcpu` reads the flag), and the
      verdict carries the names note; `sqlite3-stripped` lists no open
      finding.*
- [x] Every case runs at `perf_event_paranoid` 2 and says what that cost it:
      the stacks it lost, the sources it went without.
      *Seventeenth wave: reasoned (every cause answers, or skips both ways
      with the machine's reason first), not run; the sysctl is machine-wide
      and is the coordinator's privileged run (`STATE.md`, "Needs
      privilege").*
      *Eighteenth wave: still not run. One privilege model makes the run
      meaningful (at `718cae1` the two ways ran with different privileges and
      the agent's failed); the commands are `STATE.md`, "Needs privilege",
      item 1.*
      *After the eighteenth wave's merge, done (coordinator, 2026-10-04): with
      the capabilities every cause answers both ways with the same
      privileges, as at -1 (`a-cold-file` skips as it does at -1); without
      them every cause skips both ways with the machine's reason first
      (`CAP_BPF`, `unprivileged_bpf_disabled`, tracefs) and the skip gate
      passes over the 30 lines; a user-only profile names paranoid 2 and what
      it costs (6 to 11.5 % of `sqlite3`'s commit loop). What a case loses at
      paranoid 2 without the capabilities is every source it needs:
      `omn latency` refuses rather than degrades, because `sched.runqlat` is
      required.*
- [x] Adding a seventh program means adding a directory under `cases/`, and
      zero lines under `src/`.
      *Seventeenth wave: after the format (`docs/cases.md`), the python and
      build streams added five case directories with no line under `src/`
      and no extractor in `tests/cases.rs` (their accounts are `key=value`
      scripts beside each case); `cases::every_case_is_well_formed`
      discovers and checks every directory.*

---

## Cross-cutting test strategy

To be put in place from phase 3, not at the end. A tracer is hard to test because
it has no "obviously wrong" output.

| Level | Method |
|---|---|
| Ground truth | synthetic workloads with known behavior (`usleep`, `dm-delay`, `tc netem`, fixed-size IO) |
| Cost regression | the phase 8 harness, in CI (built, and run by hand on a quiet machine; not in CI, since its variance gate exists to refuse a loaded one) |
| Portability | a VM kernel matrix: floor, LTS, recent, no BTF (built in the eleventh wave: `scripts/matrix.sh`, `docs/portability.md`) |
| Join robustness | injection of disorder, losses, recycled pointers |
| Degradation | run without privileges, without BTF, without tracefs — check that it degrades **while saying so** (built with the matrix, one configuration each; the CI is the unprivileged cell) |

---

## Accepted boundaries, to be documented in the README from day one

1. **No bit-exact determinism.** PMU counters are stable (typical CV < 0.1 %) but
   never exact: skid, speculation, IRQs, SMT. Strict determinism belongs to
   dynamic binary translation, outside the kernel-level core.
2. **No inside view of NVIDIA GPUs.** CUPTI is closed, there is no kernel path.
   AMD and Intel are covered.
3. **Writes are less attributable than reads.** Writeback leaves in a kworker, the
   originating context is lost: `KEY` evidence, not `IDENTITY`.
4. **Single machine first.** Inter-machine clock drift is a different class of
   problem from local correlation.
5. **It observes; it does not tune.** Every knob a performance engineer reaches
   for is out of scope, and so is anything that modifies the target to measure
   it -- `ptrace` and `userfaultfd` are interception, not observation.
   `DECISIONS.md` carries the reasoning.
6. **LTTng and its kind are peers, not backends.** Same events, another path;
   nothing to recover by wrapping one. If LTTng-UST ever earns a place it is as
   a CTF **import**, a consumer of one more format.

These limits must be visible on first contact with the tool. A project named
"Omniscient" that fails to state them turns its boundaries into perceived bugs.
