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

## Landed

1. **CI red: the resident-size decomposition** (`defaults` cell, panic at
   `tests/join_engine.rs:854`: live 50 665 124 against 50 331 648 held). The
   tool was right: the first reading with `live > 0` landed inside the
   interpreter's startup on the slower VM, so the rise in `live` included
   326 KiB of `ctypes` the interpreter allocated after it. The workload now
   prints `READY <ns> <in use> <free> <rss>` (glibc's `mallinfo2` and its own
   `statm`) once its imports are done; a first reading after it is held to the
   exact figures as before, one before it to bounds the program states (each
   row may include at most what was in use, free or resident when it was
   ready), and a glibc with no `mallinfo2` says "not asserted". Simulated
   locally with a slowed startup (40 allocations over 200 ms before `READY`):
   the old assertion fails, the new passes.
   Test: `join_engine::the_resident_size_decomposes_into_live_and_retained_with_the_residue_the_program_predicts`.
2. **`iface` compared by its index** (`src/event.rs`, `src/join.rs`). A key
   with index 0 gets the kernel's index from its name at the read (the part
   before `/`); `join::identity` is `iface#<index>`, the name only without one;
   `between` keeps each reading's baseline on the full key; labels name the
   interface. Tests: `event::tests::an_interface_named_without_its_index_is_given_the_kernels`,
   `join::tests::an_interface_is_one_key_by_its_index_across_a_rename_and_a_sub_series`,
   `join::tests::a_reading_of_an_interface_keeps_its_own_baseline_and_shares_the_interfaces_states`,
   and the ground truth
   `join_engine::two_sources_of_one_interface_join_by_it_and_credit_what_lo_moved`
   (`net.iface` and `net.bytes` of `lo` by `between`, credited bytes at least
   `/sys/class/net/lo/statistics` across the test's own 2000 datagrams and at
   most across the run: 4 112 000 credited for 4 112 000 counted; before the
   fix 0 of 20 readings matched). `docs/keys.md` rewritten for `iface` and
   `proc`.
3. **`proc` kept, and given its maker**: `KeyValue::proc_of(pid)` (pid
   namespace inode, `NSpid`). Kept by alignment with `CATALOG.md`'s container
   trap; the collectors that should carry it are filed. Test:
   `event::tests::a_process_is_keyed_by_its_pid_namespace_and_its_pid_inside_it`.
4. **`overlap`'s `enclosing`** (DECISIONS "Open"): a rule names the attribute
   holding a drained row's enclosing window; a row whose window lies in one
   interval is credited whole, one that crosses stays uncredited; absent, as
   before. `enclosing` on another match is refused
   (`JoinError::EnclosingOutsideOverlap`), a row lacking the attribute is
   `MissingSpan`. Test:
   `join::tests::a_drained_row_is_credited_whole_to_the_one_interval_that_encloses_it`.
   On the reclaim crossing against the cgroup's `memory.stat`: 745 of 41 413
   pages credited today; 165 / 143 with the drain's close as the enclosure;
   35 225 of 37 657, 40 497 of 40 739, 41 909 of 42 093 with the last
   episode's end (patch to `src/bpf/mem_stall.rs` and `stall.rs`, applied
   locally, measured, reverted, filed). `cross_machine::a_memory_limit_...`
   asserts at least half credited once the session names `enclosing`.
5. **The engine's memory**: attributions folded or ranked and cut to `top` as
   states end (`Ranked`, exact), and a stream already in order walked in
   place. The load test (500 k then 1 M events): 48.7 / 97.2 MiB beyond the
   stream before, 15.4 MiB (1 M) with the attributions bounded, 0.7 / 0.7 MiB
   with both. Now asserted: every peak under 4 MiB, the long one within 1 MiB
   of the short. Test: `join_engine::a_join_under_load_ends_every_state_it_opened_and_holds_no_more_than_its_map`.
6. **The hour test** runs as its doc says; `OMN_JOIN_LOAD_SECS` shortens it,
   and each round now asserts the 4 MiB bound too. Run here for 300 s in
   release: 109 rounds of 2 001 347 events, every peak 0.7 MiB, nothing left
   live, `test result: ok`. The hour is the coordinator's, alone.

## CI expectation (defaults / stock)

- `join_engine::the_resident_size_...`: **green / green.** The shim needs no
  privilege (it ran on both cells). On `defaults` the first reading came
  before the interpreter was ready, which is now the bounded branch; on
  `stock` it passed before and either branch holds. Ubuntu 24.04's glibc
  (2.39) has `mallinfo2`.
- `join_engine::two_sources_of_one_interface_...` (new): **green / green.**
  `net.bytes` and `net.iface` are `fs:proc`, unprivileged; `lo` and
  `/sys/class/net/lo/statistics` exist on a GitHub VM. A machine with neither
  source refuses with a reason `machine_cannot_serve` reads, and the test skips
  with it.
- `event::tests` (new two): **green / green**: `/sys/class/net/lo/ifindex`
  is 1 and `/proc/self/ns/pid` is the test's own.
- `join_engine::a_join_under_load_...`: **green / green**, in-process, no
  privilege.
- Every other test of this stream's files is unchanged in what it needs.

## Open

- The engine's input is still linear outside the engine: `main.rs` clones
  every collection's events into an unsorted `merged`, so the engine takes its
  index there (12 bytes an event). Filed. True streaming during collection
  needs the runner to keep no events, which a `raw` emit cannot do.
- A rule cannot choose the kernel tier (a rule compiled into a program of
  `src/bpf/` per match kind, the shared layer) or the query tier (a join over
  stored runs, for which `omn` has no command). Cost: phase 5's "adding a
  boundary means adding a rule" stays `[~]`, the agent tier only.
- The reclaim crossing's credit waits on the filed `mem_stall.rs` field.
- `proc` waits on its producers.

## Needs privilege

- **Disk**, before anything else: `/home` was at 100 %. What a human can free
  (none of it this stream's to delete): the wave-15 worktrees' targets,
  `du -sh /home/moha/repos/omniscient-{engine,kernel,probes,resolver,runner}/target`
  (7.3 to 7.8 GiB each), `cargo clean` in each, or
  `git worktree remove` for the merged ones; and the main checkout's 25 GiB
  `target/`. Then this stream's tmpfs build can go back:
  `rm /home/moha/repos/omniscient-w16-engine/target/debug && make` in the
  worktree.
- Nothing else: the measurements above used only `make`'s setcap.

## For DECISIONS.md

- **Settled: a drained row's window is the rule's to name.** An `overlap` rule
  names `enclosing: <attr>`, the attribute holding a window, from the row's
  stamp, that encloses every occurrence; such a row is credited whole to the
  one interval holding the window, and stays uncredited when it crosses one.
  Without it, `span` is a sum and the row is uncredited. A source that does
  not carry the attribute is refused (`MissingSpan`), never read as a sum.
  Chosen over `span: enclosing` because a drained row carries both numbers,
  and a rule needs both: the sum for a row of one, the window for a row of
  several. Replaces the "Open" line.
- **Settled: an interface is its index.** A join compares `iface` by the
  kernel's index, filled from the name at the read when a producer has only
  the name; the name is what a row prints. A sub-series after `/` is not part
  of the interface.
- **Settled: `proc` stays**, by `CATALOG.md`'s container trap, made by
  `KeyValue::proc_of`; removing it would leave the trap without its answer.
- **Settled: the engine's memory is bounded by its map and its `top`**, plus
  an index of the input when the input is not in order.

## Filed for other streams

Under "Needs from another stream — the sixteenth wave, stream engine" in
`docs/open-findings.md`: the stall row's last episode end (`src/bpf/mem_stall.rs`,
`src/collect/mem/stall.rs`, with the patch and the session line); `proc` on
`proc.*` and `mem.footprint` (`src/collect/proc/status.rs`,
`src/collect/mem/footprint.rs`, `catalog/{proc,mem}.yaml`); the joins' input
in order (`src/main.rs`); a note to `src/collect/net/iface.rs`.

## Session index rows

`examples/README.md` and `GUIDE.md` (the coordinator's; `cli::the_session_indexes_hold_exactly_the_sessions_on_disk`
is red for it until then):

```
| `join-iface-loopback.yml` | Two sources of loopback joined by the interface's index, the bytes it moved credited while both watched. |
```
