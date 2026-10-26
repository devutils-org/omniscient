# The eighteenth wave, stream engine

Branch `feat/w18-engine`, from `main` at `9cd5409`. `docs/dev/BRIEF.md`'s section
"engine -- the files nobody owned": `docs/dev/STATE.md` item 6, item 8 (except
`tests/cases.rs`, `docs/cases.md` and `scenario_sched`) and item 11's engine
lines. Everything below was run on the development machine (24 cpus, Linux
7.2.7, `CAP_BPF` and `CAP_PERFMON` on `target/debug/omn` through `make`) on
2026-10-04, with nine other streams building and testing on it at the same
time.

## Landed

| Commit | What |
|---|---|
| `0a25793` | `fix(shim)`: no clock read on a call that does not report (`docs/dev/STATE.md` item 6) |
| `9970178` | `fix(bpf)`: `Btf::member_offset` follows a typedef to the struct it names |
| `7876659` | `test(join)`: the reclaim session's `enclosing:` line with its assertion; another namespace's lo |
| `81f78c1` | `docs`: the perf map under `--symbols local` in `docs/CATALOG.md`; `iface#<netns>:<index>` in `docs/families/join.md` |
| `6f5ac61` | `feat(join)`: a rule chooses its tier -- `agent`, `query` over the stored run, `kernel` refused; `join::sources_read` |
| `66f16e4` | `docs/families/join.md`, the eighteenth wave's record |

### 1. The shim's clock (`shim/src/lib.rs`, `Timer::start`)

`Timer::start` read `CLOCK_MONOTONIC` on the timed turn (one call in 17)
before it asked `ring::mapped()`, `in_hook()` and `reporting(class)`. A call
of a class nobody asked for -- every `malloc` under a `sched.mutex`-only
session -- paid that clock read on its turn. The checks now come first and the
clock is read only for a call that reports; the turn counter is untouched by a
call that does not. The cost, said in the doc comment: those few loads fall
outside a timed call's span, understating the shim's own account by a handful
of nanoseconds per timed call.

Test: `tests::a_call_that_does_not_report_reads_no_clock` counts
`monotonic_ns` calls through a `cfg(test)` thread-local and drives
`Timer::start` for every class at the timed turn with no ring mapped. Shown
failing on the old order (the clock-read assertion), passing on the new.

### 2. `Btf::member_offset` through a typedef (`src/bpf/btf.rs`, a shared layer)

A name that is a `TYPEDEF` of a struct or union (through `const`/`volatile`/
`restrict`) is now followed, by the same `composite` walk anonymous members
use. Structs and unions of the name are searched first, so a name that is
both answers from the struct as before.

Every caller (about 150 call sites, 35 distinct type names) passes a struct
name. What could move a caller's answer is a typedef of that same name naming
a different type; `no_caller_s_type_is_shadowed_by_a_typedef_on_the_running_kernel`
checks the 35 names against the running kernel's BTF and finds none. Five new
tests in all (an anonymous struct through a typedef and a `const`; a typedef of
a scalar still refused; a struct and a typedef of one name; the caller audit;
`possible_net_t.net` at 0 on this kernel, read and not skipped). `cargo test
--lib bpf::`: 390 passed, which is every in-process test of the eBPF layer
that reads offsets from the running kernel.

`net_qdisc::netns_offsets` still writes the 0 by hand: `src/bpf/net_qdisc.rs`
is nobody's this wave (below).

### 3. The reclaim session's `enclosing:` (`examples/cross-machine-reclaim-io.yml`, `tests/cross_machine.rs`)

`reclaim-per-pressure-interval` names `enclosing: enclosing_ns`, its comment
block says what that does, and
`cross_machine::a_memory_limit_turns_a_cached_read_into_device_reads_and_each_byte_carries_its_rung`
asserts at least half the cgroup's pgsteal is credited in the intervals the
program overlapped. The measured run the brief asked for first, with the
capabilities, on this branch: **42 314 of 42 314 pages credited, 0
uncredited**. Half is the separation; the seventeenth wave's three runs
(38 967 of 38 967, 42 668 of 42 668, 39 388 of 39 572) and this one are all at
or near the whole.

`tests/cross_machine.rs` is in no stream's row; the brief's section names
"with its assertion", so it is changed here, by these lines only.

### 4. Another namespace's lo (`tests/join_engine.rs`)

The seventeenth wave's `another_namespaces_lo_is_not_credited_to_this_ones`,
appended as filed. Run once with the capabilities: **their namespace
4026532813: credited 0, uncredited 2000, 5 rows of theirs**. It skips with the
machine's reason where `unshare -rn` or `netem` fails.

### 5. `docs/families/join.md:39` and `docs/CATALOG.md`'s `--symbols` lines

The identity is `iface#<netns>:<index>` (`iface:<netns>:<name>` when the
index stayed 0), with why. `docs/CATALOG.md` said `--symbols debuginfod` was "the
mechanism landed, the flag not yet"; the flag is in `src/cli.rs`. It now says
what `--symbols local` reads -- the mapped files, `/proc/kallsyms`, and the
`/tmp/perf-<pid>.map` for an anonymous executable mapping -- and what
`debuginfod` adds.

### 6. A rule chooses its tier (phase 5's last `[~]`)

`Rule` gains `tier: agent | query | kernel`, `agent` by default and not
written when it is the default.

- **`query`**: `join::apply_stored(rule, &[Results])` applies the rule to a
  stored run's `raw` outputs with the same `apply_merging`, each source's
  window merged the way `assemble` merges it, closed at its detach when its
  stored `stopped_because` says it was detached and watched to the end
  otherwise (the review's round, below). `check_in` refuses, at validation, a
  query-tier rule whose side is not collected or is collected without `raw`.
  `JoinError::NotStored` names the side and the reason.
- **`kernel`**: refused by `check` with the reason and the two tiers that
  serve it (`JoinError::KernelTier`). No match kind has a program of
  `src/bpf/`.

Ground truth: `join_engine::a_rule_moved_to_the_query_tier_answers_from_the_stored_run_what_the_agent_did`
runs one `eq` rule twice in one session (`proc.threads` to `proc.fd`, both
`raw`), once per tier, reads the JSON's `results` back as `Vec<Results>`, and
asserts `apply_stored`'s correlation equals the agent tier's field for field
(rule id aside), with `matched > 0`. It holds whether or not the run also
applies the query-tier rule, so it survives the `src/main.rs` change below.
`a_tier_the_run_cannot_serve_is_refused_before_anything_is_attached` holds the
two refusals at `omn validate` (exit 2, with what to do instead). Five unit
tests in `src/join.rs`.

### 7. `assemble`'s linear clone: `join::sources_read`

`join::sources_read(joins, decompositions)` names the sources whose events a
run has to keep beside its results: every agent-tier rule's two sides, every
decomposition's measured whole and parts, and the sides of the rule a `paired`
part names. `a_decomposition_over_the_sources_it_reads_is_the_one_over_every_source`
holds that a table read from those events alone equals the table read from
all of them, and that an unread source is left out. Using it is `src/main.rs`
(below).

## Shown versus reasoned

Shown, on this machine:

- the shim test failing on the old order and passing on the new;
- `possible_net_t.net` at offset 0 through the typedef, and no caller's type
  shadowed, on the running kernel (7.2.7); the 390 in-process eBPF-layer tests;
- 42 314 of 42 314 pages credited by the 100 ms overlap with `enclosing:`;
- credited 0, uncredited 2000 for another namespace's lo;
- the query tier's correlation equal to the agent tier's on a real run;
- with the `src/main.rs` diff below applied (then reverted): `tests/join_engine.rs`
  whole, 26 passed; and a three-second machine-wide `sched.switches` run with
  no join peaking at 0.96, 0.99 and 1.26 KiB resident per event, against 1.58,
  1.57 and 1.92 without it. Event counts ranged from 51 k to 369 k with nine
  other streams loading the machine, so that is per event and indicative: the
  two sets do not overlap, and the size of the saving is not a calibration.

Reasoned, not measured:

- the shim's saving on a real program. The seventeenth wave's 20 M
  `malloc`/`free` pairs (0.12 s bare, 1.07 s under the release shim) were
  measured with the shim's heap class unreported; this change removes the
  clock read from that path, but the pairs were not re-run here (they want a
  quiet machine and the release shim, which is the coordinator's run below).
- that no `member_offset` caller's answer moved on a kernel other than this
  one: the audit test runs wherever the suite does, the matrix will run it on
  the others.

## Could not do, and why

- **The kernel tier.** A program of `src/bpf/` per match kind is the shared
  eBPF layer and a stream of its own; refused with the reason instead.
- **The run skipping a query-tier rule, and cloning only what is read**: both
  in `src/main.rs`, stream launcher's. The diff is below, applied, built,
  tested and reverted.
- **A command that applies a rule to a stored run**: `src/cli.rs`, stream
  names'. Until it lands the query tier is the library's.
- **`net_qdisc::netns_offsets` asking `member_offset("possible_net_t", "net")`**:
  `src/bpf/net_qdisc.rs` is in no row this wave.
- **`docs/unwind.md`'s perf-map line**: stream profile's (below).

## The commit-time run

`make check` once, at `66f16e4`: fmt and clippy clean, and the suite red,
fail-fast, on two tests outside this stream's paths:
`cross_io::the_crossing_sessions_run_and_say_what_they_found` ("the one-liner
issues 2000 direct reads and the scoped join matched 3995": the flaky tally's
25, whose two machine-wide sessions are each other's neighbour, here with nine
streams beside them) and `cases::every_case_names_its_known_cause_both_ways`
(`sqlite3-durability` and `cpython-interpreter`, the cases stream's and the
triage's).

Then one `cargo test --workspace --no-fail-fast`. Its log was clobbered by a
test that rewrites the terminal (only the last 14 binaries survived), and
its tail named 8 targets red: `cases`, `cross_io`, `family_sched`, `recorder`,
`runner`, `scenario_net`, `triage`, `unwind`, with `scenario_mem`'s
`a_walk_that_fits_in_l1_...` (tally) among the surviving output. Each target
rerun alone:

| Target | Alone | Then |
|---|---|---|
| `recorder`, `family_sched`, `cross_io` | green | -- |
| `scenario_net::every_datagram_sent_to_a_closed_port_is_a_drop_with_one_reason` | red (`NO_SOCKET` 7 of 512, `SOCKET_RCVBUFF` 1779: another test's flood) | green alone, once |
| `runner::a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed` | red (91 980 against 90 147) | **red on `main`'s `src/` and `shim/` too** |
| `triage::a_verdict_read_off_a_lossy_profile_says_its_amount_is_a_lower_bound` | red (limit `None`, not `cpu`) | **red on `main`'s `src/` and `shim/` too** |
| `unwind::dwarf_recovers_the_whole_tree_of_a_build_without_frame_pointers` | red (52 of 2034 samples cut) | red again alone; no result line on `main`'s sources |
| `cases` | not rerun: phase B's, and 5 minutes of the machine |

The `main` check was `git checkout main -- src shim`, `make`, the three
tests, and back. None of the red tests reaches the code this stream changed
except through `member_offset`, whose callers' types the audit test shows
unshadowed. The last three are cpu-profile tests under the load of ten
streams; they are for the profile and triage streams' tallies, not this one's.

This stream's own: `cargo test --lib join::` 84 passed, `--lib bpf::` 390,
`-p omn-preload` 15, `--test join_engine` 26 (one ignored, the hour), the
reclaim crossing and the namespace test green with the capabilities.

### The review round's run, at `7cc2f79`

`make check` once: the suite red, fail-fast, on
`cases::every_case_names_its_known_cause_both_ways` alone
(`sqlite3-durability/commits-that-wait-for-nothing`'s
`both-ways-name-the-same-suspect`, `_raw_spin_unlock_irqrestore` against
`bpf_trace_run2`: a flat on-cpu leaf under the load of the other streams, the
tally's kind). Then `cargo test --workspace --no-fail-fast`: **2198 passed, 9
failed, 10 ignored**; clippy `-D warnings` and `fmt --check` clean. The nine:
`cases` (as above, leaves swapped), and the two red on `main`'s sources before
(`runner::a_profile_s_losses_...`, `triage::a_verdict_read_off_a_lossy_profile_...`);
the other six each green rerun alone once:
`budget::the_extra_cpu_a_faster_poll_costs_...`,
`cross_compute::a_half_blocked_task_decomposes_...`,
`triage::the_same_program_is_cpu_bound_...`,
`triage::a_program_waiting_for_a_mutex_...`,
`unwind::a_stripped_build_is_named_from_debuginfod_...`,
`scenario_net::every_datagram_sent_to_a_closed_port_...`. While iterating:
`--lib join::` 85 passed, `--lib collect::tests::a_stored_reason...` and the
BTF audit green, `--test join_engine another_namespaces` green with the
capabilities.

## Privileged runs wanted

None that changes a sysctl or loads a module. Alone, with the capabilities, on
`main` after the merge:

```console
make
cargo test --test cross_machine a_memory_limit_turns -- --nocapture
cargo test --test join_engine another_namespaces_lo_is_not_credited_to_this_ones -- --nocapture
```

And the shim's saving on a quiet machine, with the release shim (the
seventeenth wave's build stream's 20 M pairs): a `sched.mutex`-only session
over a `malloc`/`free` loop, before and after `0a25793`, comparing the
target's cpu time.

## For docs/DECISIONS.md

- **A join rule chooses its tier per rule** (`tier: agent | query | kernel`,
  `agent` by default). `query` applies the same engine to a stored run's `raw`
  events and is refused at validation unless both sides are stored in full
  (collected, with `raw`), reading a detach back from each result's
  `stopped_because`; `kernel` is refused with the reason until a
  match kind has a program. Why: phase 5 decided placement per rule and asked
  that a join move between tiers without rewriting the collector; a tier that
  cannot answer what the agent would have is refused rather than degraded.
- **`Btf::member_offset` follows typedefs, structs first.** Why: the loader
  should check what `net_qdisc` wrote by hand, and C names a typedef's members
  the way it names a struct's.

## Review

The adversarial review's round, in `7cc2f79`.

- **Defect, the namespace test's skip.** The sender's failure printed
  "unprivileged network namespace", which neither `.github/expected-skips.txt`
  nor the stock cell's list matches. The helper now probes `unshare -rn true`
  before it starts the session and skips with "this needs an unprivileged
  user namespace and this machine allows none (`unshare -rn`)": the stock
  cell's existing "an unprivileged user namespace". Each step of the sender
  exits with its own code; `tc ... netem` failing (exit 8, a kernel without
  `sch_netem`, which the defaults cell may be) skips with "this machine cannot
  serve a `netem` discipline ...": the common list's "cannot serve". Any other
  failure of the sender, inside a namespace the machine let it make, fails
  the test. Not run on a cell: green's next CI read is what shows it. Nothing
  under `.github/` changed.
- **Gap, `apply_stored` and a detach.** Fixed by reading it rather than
  refusing it: every way a collection is detached (its window, the budget,
  the escalation) now spells its `stopped_because` through one function,
  `collect::detached_at`, and `collect::stopped_at_a_detach` reads it back
  (`collect::tests::a_stored_reason_says_whether_its_collection_was_detached`).
  `apply_stored` keeps a detached side's stored `closed_ns`, as `assemble` does
  for a source in its `detached` set, so the query tier answers what the agent
  tier did over any stored run, the `omn join` command stream names is asked
  for included. With that, `check_in`'s refusal of a windowed request had no
  reason left and is gone (a window's start is the stored `opened_ns`, its
  stop a detach). `join::tests::a_stored_side_detached_mid_run_is_closed_where_it_was_detached`
  asserts the detach changes the answer for its events and that the stored
  run gives the run's. The doc comments and `docs/families/join.md` say so.
- **Nit, the BTF audit.** A caller's name that is only a typedef now fails it
  (following it would turn a refusal into an offset), and a name absent from
  the kernel's BTF is printed rather than passed over. Green on 7.2.7, none
  absent.
- **Nit, the namespace test's assertions.** The OR is two assertions: a raw
  row of the queued result carries their `netns`, and the queued result's
  series is keyed `iface:lo@netns:<inode>` (the join itself names no row of
  theirs: its `top` is empty and the 2000 are `uncredited`, so the old first
  branch could never have held; the doc comment's "every series" is the
  result's). Both halves now assert that `net.qdisc`'s window held the
  sender's burst, which the sender stamps with `CLOCK_MONOTONIC`: a zero is
  then about the burst, not about when the session watched. Run with the
  capabilities: credited 0, uncredited 2000, 3 rows of theirs, both halves
  green.
- **Nit, `tests/cross_machine.rs`.** Acknowledged above (section 3); for the
  coordinator, unchanged.
- **Nit, `docs/CATALOG.md`.** Now quotes the facts line as `src/unwind.rs` and
  `src/stack.rs` write it: `names came from -- ... perf-<pid>.map: perf-map,
  the <n> lines the program wrote for code no file holds`.

## Needs from another stream

### To stream launcher (`src/main.rs`, `assemble`): skip a query-tier rule, and clone only what is read

Applied, built, `tests/join_engine.rs` run whole (26 passed) and reverted.
rustfmt may rewrap the `for` line.

```diff
--- a/src/main.rs
+++ b/src/main.rs
@@ fn assemble(
     let mut bounds = vec![0];
+    // Only the sources a join or a decomposition reads: every other
+    // collection's events are consumed by `aggregate` and never cloned.
+    let read = join::sources_read(&session.joins, &session.decompose);
     let mut run = encode::RunOutput {
@@
-        merged.extend(collection.events.iter().cloned());
+        if read.contains(resolved[index].request.source.as_str()) {
+            merged.extend(collection.events.iter().cloned());
+        }
         bounds.push(merged.len());
@@
-    for rule in &session.joins {
+    // A query-tier rule is applied to the stored run (`join::apply_stored`),
+    // not here.
+    for rule in session.joins.iter().filter(|rule| rule.tier == join::Tier::Agent) {
         run.joins
             .push(join::apply_merging(rule, &streams, &windows)?);
     }
```

### To stream names (`src/cli.rs`, `man/omn.1`): apply a rule to a stored run

A subcommand, say `omn join -f <session-or-rule.yml> <results.json>`, that
reads the stored `results` as `Vec<omn::emit::Results>`, applies each
query-tier rule with `join::apply_stored`, and prints the correlations the
way a run does. `apply_stored` reads a detach back from each result's
`stopped_because`, so a stored run with a window or a detach needs no refusal
of its own there.

### To stream green (`.github/**`): nothing to file

`another_namespaces_lo_is_not_credited_to_this_ones` skips with "an
unprivileged user namespace" (the stock cell's list) or "cannot serve" (the
common list); a CI read that shows another wording from it is this stream's
defect, not a pattern to add. And `man/omn.1`'s description of a join rule wants the
`tier:` line.

### To stream profile (`docs/unwind.md`): the perf map under `--symbols local`

`--symbols local` (and so `debuginfod`) reads `/tmp/perf-<pid>.map` for an
address in an anonymous executable mapping no file names; a frame it named
says `perf-map`. `docs/CATALOG.md` says so now; `docs/unwind.md` does not yet.

### To whoever owns `src/bpf/net_qdisc.rs` next: ask for the `net` pointer

`netns_offsets` can read `btf.member_offset("possible_net_t", "net")` and
add it to `nd_net`'s offset instead of writing 0, and its doc comment's "does
not follow a typedef" is no longer true.

### To the coordinator (`docs/ROADMAP.md` phase 5, `docs/dev/STATE.md`)

Phase 5's first criterion: the query tier is served and tested, the kernel
tier refused with the reason; `[~]` for the kernel tier only. `docs/dev/STATE.md` items
6 and 8 (this stream's lines) and item 11's "a rule choosing the kernel or
query tier" and "`assemble`'s linear clone" (half: the diff above) can move.
