# The sixteenth wave, stream engine

Branch `feat/w16-engine`, from `main` at `37f17c8`. The engine's record of
the wave is `docs/families/join.md`, "The sixteenth wave"; this is the handoff.

## The machine this wave ran on

`/home` (one btrfs volume shared by every worktree, the old wave-15 worktrees'
`target/` directories and `/.snapshots`) was full for most of the wave: two
`make check` runs died with `No space left on device` and a linker `SIGBUS`,
and `git` could not write its index for a while. This stream moved its
`target/debug` into tmpfs (`/tmp/claude-1000/w16e-debug`, symlinked) to keep
building. `/tmp` is mounted `nosuid`, so the binary loses its file
capabilities there: the commit-time full runs were made without `CAP_BPF`, and
the eBPF tests of this stream's files skipped loudly in them. The privileged
paths were exercised when `/home` had room: the reclaim crossing below
(`CAP_BPF` granted by `make`, measured three times with and without the
enclosure), `family_sched` and `portability` green with capabilities. **A
human should free space before the merge** (see "Needs privilege"), and the
coordinator's merge run is the first full privileged run of this branch.

The first 300 s run of the hour test was killed after 17 rounds with no
assertion message, while the machine had 23 of 31 GiB in use and 12 GiB of
tmpfs; the rerun of the same binary passed (below). Read as the machine's
memory, not the engine's: every round's peak was 0.7 MiB beyond its stream.

## What the commit-time runs said

One `cargo test --workspace --no-fail-fast` per commit (each `make check`
stopped at the expected index failure), then clippy and fmt, green. Red in the
second run, none in this stream's files:

- `cli::the_session_indexes_hold_exactly_the_sessions_on_disk`: the new
  session's index row, expected (below).
- Five `scenario_io` tests: `No space left on device` writing their files
  under `target/tmp` on the full `/home`.
- `scenario_mem::both_huge_page_backends_read_the_same_promotion_counter`
  (tally #21) and `unwind::each_mode_copies_what_it_declares_and_dwarf_costs_more_per_sample_than_fp`
  (the fifteenth wave's tally): each failed again alone, once, at a load
  average of 27 to 46. Not investigated here: neither touches `join` or
  `event`.
- With capabilities afterwards, this stream's test binaries run directly
  (`vertical` 16, `join_engine` 22, `cross_compute` 8, `cross_io` 7,
  `cross_machine` 5, `cross_memory` 9): all green, except
  `cross_compute::a_wakeup_join_onto_drained_totals_is_refused_because_each_event_holds_many_blocks`
  once inside its binary's run, which passed alone twice: for the flaky tally.

## The review, and what changed after it

The reviewer's five findings, each checked:

1. **`enclosing` held to no ground truth -- real.** The assertion in
   `cross_machine::a_memory_limit_...` ran only once the session named
   `enclosing:`, which it could not until another stream's field landed: dead
   everywhere. Fixed without that field: `mem.reclaim`'s `window_ns`, from the
   row's stamp (its first episode), already encloses every episode of the row,
   so `enclosing: window_ns` is sound today. New session
   `examples/cross-machine-reclaim-enclosed.yml` (the pressure read every
   second, so a window of at most 200 ms lies in one reading) and new test
   `cross_machine::a_drained_reclaim_row_is_credited_whole_inside_one_second_of_pressure`,
   held to the cgroup's `memory.stat`. Four runs here, with `CAP_BPF`:
   41 797, 41 943, 42 263, 41 246 pages credited of as many counted by
   pgsteal (0 uncredited), against 437, 197, 294, 377 for the same rows read
   as a sum. The dead assertion is gone; the filed stall field is now a
   tightening for the 100 ms session, not a precondition.
2. **The iface test never exercised a kernel index -- real.** Both of
   `net.bytes` and `net.iface` resolve the index from a name, so the test held
   the `/drops` suffix only, and the handoff did not say `net.bytes` replaced
   the brief's `net.qdisc`. New session `examples/join-iface-qdisc.yml` and
   test `join_engine::a_queued_packet_on_lo_joins_lo_s_link_counters_by_its_index`:
   `net.qdisc` (the kernel's index) against `net.iface` (a name) by `overlap`,
   held to `/sys/class/net/lo/statistics/tx_packets`. **It skips here and on
   both CI cells**: lo is `noqueue`, so no packet of it is queued and
   `net.qdisc` has nothing to key -- the brief's pair cannot be served on lo
   without root's `tc`. Measured why a fifo does not help: `pfifo` and
   `fq_codel` on a namespace's lo, 2000 datagrams, zero rows (bypassed while
   empty); `netem limit 100000`, 2000 rows. Exercised by hand on the lo of an
   unprivileged namespace (`unshare -rn`): all 2000 packets credited to
   `iface:lo` by index 1 -- which also shows **a defect of the index
   identity**: that lo was another namespace's. Filed (the namespace beside
   the index, kernel-side producers), and `docs/keys.md` says when such a join
   is sound.
3. **`proc` still has no producer -- real; the earlier wording overclaimed.**
   `KeyValue::proc_of` is a constructor nothing calls. Not landed: `proc` stays
   as vocabulary, the way a source with no collector does, and a join `via:
   proc` is refused before the run by name ("`proc.threads` does not carry"
   it, checked here). The producers are other streams' files, filed.
4. **"The engine's memory no longer grows with the run" overclaimed -- real.**
   True only of input in order; `assemble` hands the engine the collections
   laid end to end, so a real run took the index as before. Added
   `join::apply_merging(rule, &[&[Event]], windows)`: the collections walked
   apart by a cursor each, no index of an in-order collection, the order a
   stable sort of their concatenation gives
   (`join::tests::streams_walked_apart_meet_in_the_engines_order`, worked out
   by hand). The load test now measures the input as a run hands it over:
   two collections of a million events walked apart, 0.6 MiB beyond them;
   laid end to end (what `assemble` does today), 15.3 MiB. A real run gets the
   former only once `assemble` passes its collections' slices -- three lines
   of `src/main.rs`, filed with the lines. The commit title of `8e244ae` is
   not rewritten; this is the correction.
5. **The lib unit tests unverified -- resolved.** `/tmp` and `/home` had room
   again; `cargo test --lib -- join:: event::` rebuilt and ran, 83 passed,
   the four new ones among them, before any change of this round; 84 with the
   merge test after it.

## Landed

1. **CI red: the resident-size decomposition** (`defaults` cell, panic at
   `tests/join_engine.rs:854`: live 50 665 124 against 50 331 648 held). The
   tool was right: the first reading with `live > 0` landed inside the
   interpreter's startup on the slower VM, so the rise in `live` included
   326 KiB of `ctypes` the interpreter allocated after it. The workload now
   prints `READY <ns> <in use> <free> <rss>` (glibc's `mallinfo2` and its own
   `statm`) once its imports are done; a first reading after it is held to the
   exact figures as before, one before it to bounds the program states, and a
   glibc with no `mallinfo2` says "not asserted". Simulated locally with a
   slowed startup: the old assertion fails, the new passes.
   Test: `join_engine::the_resident_size_decomposes_into_live_and_retained_with_the_residue_the_program_predicts`.
2. **`iface` compared by its index** (`src/event.rs`, `src/join.rs`). A key
   with index 0 gets the kernel's index from its name at the read (the part
   before `/`); `join::identity` is `iface#<index>`, the name only without one;
   `between` keeps each reading's baseline on the full key; labels name the
   interface. Tests: `event::tests::an_interface_named_without_its_index_is_given_the_kernels`,
   `join::tests::an_interface_is_one_key_by_its_index_across_a_rename_and_a_sub_series`,
   `join::tests::a_reading_of_an_interface_keeps_its_own_baseline_and_shares_the_interfaces_states`;
   ground truth for two named producers,
   `join_engine::two_sources_of_one_interface_join_by_it_and_credit_what_lo_moved`
   (`net.iface` and `net.bytes`, 4 112 000 bytes credited for 4 112 000 the
   kernel counted); for a kernel index against a name,
   `join_engine::a_queued_packet_on_lo_joins_lo_s_link_counters_by_its_index`,
   **which skips wherever lo queues nothing, this machine included** (above).
   `docs/keys.md` rewritten for `iface` and `proc`.
3. **`overlap`'s `enclosing`** (DECISIONS "Open"): a rule names the attribute
   holding a drained row's enclosing window; a row whose window lies in one
   interval is credited whole, one that crosses stays uncredited; absent, as
   before. `enclosing` on another match is refused
   (`JoinError::EnclosingOutsideOverlap`), a row lacking the attribute is
   `MissingSpan`. Tests:
   `join::tests::a_drained_row_is_credited_whole_to_the_one_interval_that_encloses_it`
   and, against `memory.stat`,
   `cross_machine::a_drained_reclaim_row_is_credited_whole_inside_one_second_of_pressure`
   (100 % of pgsteal credited in four runs, under 1.1 % read as a sum).
4. **The engine's memory**: attributions folded or ranked and cut to `top` as
   states end (`Ranked`, exact); `join::apply_merging` walks a run's
   collections apart with no index of them; `apply_within` is it over one
   stream. Load test, beyond the input: 48.7 / 97.2 MiB before (500 k / 1 M),
   0.7 / 0.7 MiB for one ordered stream, 0.6 MiB for two collections walked
   apart, 15.3 MiB for the same laid end to end. Asserted: every peak under
   4 MiB, the long within 1 MiB of the short, the collections apart under
   4 MiB, end to end at least the index's 8 MB. Tests:
   `join_engine::a_join_under_load_ends_every_state_it_opened_and_holds_no_more_than_its_map`,
   `join::tests::streams_walked_apart_meet_in_the_engines_order`. **Not in a
   real run until `assemble` calls it** (filed).
5. **The hour test** runs as its doc says; `OMN_JOIN_LOAD_SECS` shortens it,
   and each round asserts the 4 MiB bound too. Run here for 300 s in release
   before this round: 109 rounds of 2 001 347 events, every peak 0.7 MiB,
   nothing left live, `test result: ok`. The hour is the coordinator's, alone.
   This round did not change what it runs (`one_load`, one ordered stream).

## CI expectation (defaults / stock)

- `join_engine::the_resident_size_...`: **green / green.** The shim needs no
  privilege (it ran on both cells). On `defaults` the first reading came
  before the interpreter was ready, which is now the bounded branch; on
  `stock` it passed before and either branch holds. Ubuntu 24.04's glibc
  (2.39) has `mallinfo2`.
- `join_engine::two_sources_of_one_interface_...`: **green / green.**
  `net.bytes` and `net.iface` are `fs:proc`, unprivileged; `lo` and
  `/sys/class/net/lo/statistics` exist on a GitHub VM.
- `join_engine::a_queued_packet_on_lo_...` (new): **skip / skip**, with the
  resolver's reason first (`net.qdisc` needs `CAP_BPF and CAP_PERFMON`, in
  `.github/expected-skips.txt`). With the capabilities and a `noqueue` lo it
  skips with "no packet of lo was queued", which no cell reaches.
- `cross_machine::a_drained_reclaim_row_...` (new): **skip / skip**. On
  `defaults` the cgroup is created and `mem.reclaim` is refused for
  `CAP_BPF and CAP_PERFMON` (expected); on `stock` no cgroup can be created
  ("cannot create a cgroup under", in `expected-skips-stock.txt`).
- `join_engine::a_join_under_load_...` and the lib tests: **green / green**,
  in-process, no privilege.
- Every other test of this stream's files is unchanged in what it needs.

## Open

- **`assemble` still indexes the whole run** for every join, and clones every
  collection: `apply_merging` is ready, the three lines of `src/main.rs` are
  filed. The clone stays linear until `emit::aggregate` takes a collection
  by reference; streaming during collection needs the runner to keep no
  events, which a `raw` emit cannot do.
- **A kernel index against a name is untested on any machine reachable
  here**: lo is `noqueue`, and no other interface carries traffic the test may
  make without the network (`tailscale0` moved 0 packets in 5 s). Needs root's
  `tc` (below).
- **An `iface` identity can join two namespaces' interfaces** (measured, 2000
  of 2000 packets of another namespace's lo credited to this one's). Filed to
  the kernel-side producers; the engine adds the namespace to the identity on
  request.
- **`proc` has no producer**; filed.
- A rule cannot choose the kernel tier (a rule compiled into a program of
  `src/bpf/` per match kind, the shared layer) or the query tier (a join over
  stored runs, for which `omn` has no command). Cost: phase 5's "adding a
  boundary means adding a rule" stays `[~]`, the agent tier only.

## Needs privilege

- **To run the kernel-index iface test for real** (on the dev machine, after
  `make`):

  ```console
  sudo tc qdisc replace dev lo root netem limit 100000
  cd /home/moha/repos/omniscient-w16-engine && target/debug/deps/join_engine-* a_queued_packet_on_lo --nocapture
  sudo tc qdisc del dev lo root
  ```

  (or `cargo test --test join_engine -- a_queued_packet_on_lo` after a `make`
  that follows the build: here `target/debug/deps` is in tmpfs, so cargo
  copies `omn` back on every `cargo test` and the copy has no capabilities).
  `netem` with no delay or loss changes nothing but that lo's packets are
  queued; every local connection passes through it while it is set.
- **Disk**, before the merge: `/home` was at 100 % for most of the wave (98 %
  at the end). What a human can free (none of it this stream's to delete):
  the wave-15 worktrees' targets,
  `du -sh /home/moha/repos/omniscient-{engine,kernel,probes,resolver,runner}/target`
  (7.3 to 7.8 GiB each), `cargo clean` in each, or `git worktree remove` for
  the merged ones; and the main checkout's 25 GiB `target/`. Then this
  stream's tmpfs build can go back:
  `rm /home/moha/repos/omniscient-w16-engine/target/debug && make` in the
  worktree. `/tmp` is a per-user quota too (`EDQUOT` once this round).

## For DECISIONS.md

- **Settled: a drained row's window is the rule's to name.** An `overlap` rule
  names `enclosing: <attr>`, the attribute holding a window, from the row's
  stamp, that encloses every occurrence; such a row is credited whole to the
  one interval holding the window, and stays uncredited when it crosses one.
  Without it, `span` is a sum and the row is uncredited. A source that does
  not carry the attribute is refused (`MissingSpan`), never read as a sum.
  Chosen over `span: enclosing` because a drained row carries both numbers,
  and a rule needs both: the sum for a row of one, the window for a row of
  several. `window_ns` is such a window for every drained source stamped at
  its first occurrence. Replaces the "Open" line.
- **Settled: an interface is its index**, within one network namespace. A join
  compares `iface` by the kernel's index, filled from the name at the read
  when a producer has only the name; the name is what a row prints. A
  sub-series after `/` is not part of the interface. **Open**: the namespace
  beside the index, for the kernel-side producers that read every namespace.
- **Settled: `proc` stays as vocabulary** with no producer, by `CATALOG.md`'s
  container trap; a join on it is refused by name until a source declares it.
- **Settled: the engine's memory is bounded by its map and its `top`**, plus
  an index of any input stream that is not in order; a run's collections are
  handed to it apart (`apply_merging`), never laid end to end.

## Filed for other streams

Under "Needs from another stream — the sixteenth wave, stream engine" in
`docs/open-findings.md`: a tighter enclosing window for the stall rows
(`src/bpf/mem_stall.rs`, `src/collect/mem/stall.rs`, with the patch and the
session line); `proc` on the per-process sources
(`src/collect/proc/status.rs`, `src/collect/mem/footprint.rs`,
`catalog/{proc,mem}.yaml`); the joins' input walked apart (`src/main.rs`, the
three lines); the network namespace beside the index
(`src/collect/net/{qdisc,capture,hwts}.rs`, `src/bpf/net_qdisc.rs`); a note to
`src/collect/net/iface.rs`.

## Session index rows

`examples/README.md` and `GUIDE.md` (the coordinator's; `cli::the_session_indexes_hold_exactly_the_sessions_on_disk`
is red for them until then):

```
| `join-iface-loopback.yml` | Two sources of loopback joined by the interface's index, the bytes it moved credited while both watched. |
| `join-iface-qdisc.yml` | Queued packets laid across their interface's link readings: a kernel index meeting a name; lo queues nothing by default. |
| `cross-machine-reclaim-enclosed.yml` | A memory limit's drained reclaim rows credited whole to the second of pressure that encloses them. |
```
