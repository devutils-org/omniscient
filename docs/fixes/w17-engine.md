# The seventeenth wave, stream engine

Branch `feat/w17-engine`, from `main` at `8725ae0`. The three engine filings
of the sixteenth wave (`docs/open-findings.md`, "Needs from another stream --
the sixteenth wave, stream engine"). All three landed. Everything below was
run on the development machine (3900X, 24 cpus, Linux 7.2.7, paranoid -1,
`CAP_BPF` and `CAP_PERFMON` through `make`) on 2026-10-03, with the other
streams' builds and tests running on it at the same time.

## Landed

1. **A `last_ns` per stall row** (`2c9c79d`; `src/bpf/mem_stall.rs`,
   `src/collect/mem/stall.rs`, `catalog/mem.yaml`). This is the filed diff
   as written:
   - `VALUE_LAST_NS = 48`, `ROW_VALUE_BYTES` 56, and every stack slot below
     the row value moved down 8.
   - The end program stores r8 (the episode's end) into an existing row with
     a plain `stx` after the atomic adds, and into a new row's value on the
     stack beside `VALUE_FIRST_NS`.
   - `StallRow::last_ns` is decoded from the row.
   - Every row a drain emits carries `enclosing_ns`: `last_ns - ts` when the
     kernel recorded an end after the stamp, and the drain's close minus the
     stamp otherwise.

   Two choices the filing did not make:
   - **A `count` run's rows carry no `enclosing_ns`.** They are summed over
     the run and stamped at its close, so they have no window that starts at
     the stamp. A rule that names the attribute on them is refused by the
     join (`MissingSpan`) rather than given a zero-length window.
   - **The plain store can race.** Two cpus that end episodes of the same row
     at the same moment can leave the earlier of the two ends in the row. Every
     value stored is a real end, so the window misses only the episode that
     lost the race, and by less than that episode's own duration. This is
     written beside the constant.

   The stall caveat ("an event is one bucket of one window...") had landed
   on `mem.thp` by mistake in `6c259b7`. It now sits on `mem.reclaim` and
   `mem.compaction`, the two entries it describes, and says what
   `enclosing_ns` is.
2. **`proc` carried** (`b44d461`; `src/collect/proc/status.rs`,
   `src/collect/mem/footprint.rs`, `catalog/{proc,mem}.yaml`, `docs/keys.md`).
   - `proc.fd`, `proc.threads`, `mem.rss`, `mem.pss` and `mem.uss` call
     `KeyValue::proc_of(pid)` once, at attach, and push the result after
     `task` on every event.
   - Their entries list `proc` in `keys:`.
   - A process whose namespace link is closed to this user carries `task`
     alone.
3. **The network namespace beside the index** (`94aab32`; `src/event.rs`,
   `src/join.rs`, `src/bpf/net_qdisc.rs`, `src/collect/net/{qdisc,dev,
   capture,hwts}.rs`, `catalog/net.yaml`, `docs/keys.md`).
   - **The key.** `KeyValue::Iface` gains `netns` (serde default 0, skipped
     when 0). `KeyValue::iface` fills in this process's namespace
     (`event::own_netns`, the inode of `/proc/self/ns/net`, read once).
     `KeyValue::iface_in(netns, index, name)` is for kernel-side producers.
     `iface_as_given` leaves the namespace 0, which means unknown.
   - **The identity.** It is `iface#<netns>:<index>`, or `iface:<netns>:<name>`
     when no index is known. `Display` prints the name as before, and appends
     `@netns:<inode>` only when the namespace is not this process's.
     `join::label_of` follows `Display`.
   - **`net.qdisc` reads the namespace in the kernel.** It reads
     `dev->nd_net.net->ns.inum` (`KEY_NETNS = 12`, `ROW_KEY_BYTES` 16), so two
     namespaces' devices with the same index are two rows. An `--iface` target
     compares the namespace inside the program, against a 64-bit register,
     because an inode is above `i32::MAX`. The event also carries `netns` as
     an attribute, which the catalog adds as a filter field. Where the row has
     no name, an index from another namespace is not looked up in this one.
   - **The other producers already read in this namespace.** `net.capture`
     reads through an `AF_PACKET` socket and `net.hwts` through ethtool
     netlink, and each sees only the namespace its socket was opened in. So
     they keep `KeyValue::iface`, which now carries this process's namespace,
     and a comment says why.
   - **`possible_net_t`.** It is a typedef of an anonymous struct, and
     `Btf::member_offset` does not follow a typedef. So the `net` pointer's
     offset inside it is taken as 0, its only member under `CONFIG_NET_NS`.
     Where the kernel has no network namespaces (`own_netns() == 0`), nothing
     is read and every key's namespace is 0. This is filed below.

## Shown versus reasoned

**Shown**, by a test or a run:

- **Stall rows.**
  - `bpf::mem_stall` (15 tests) and `collect::mem::stall` (11) pass.
  - New tests:
    - `the_last_end_is_written_on_a_new_row_and_on_an_existing_one`: one
      store through r0, one onto the stack, both from r8.
    - `a_rows_fields_survive_the_round_trip`: now checks `first_ns` and
      `last_ns`.
    - `a_drained_rows_window_ends_at_its_last_episode_or_else_at_the_drain`.
  - **The 100 ms session, with the follow-up applied and reverted.** I added
    `enclosing: enclosing_ns` to `reclaim-per-pressure-interval` in
    `examples/cross-machine-reclaim-io.yml`, and an assertion that half the
    cgroup's pgsteal is credited, in
    `cross_machine::a_memory_limit_turns_...`. Three runs (with capabilities):

    | Credited in the program's intervals | Uncredited | Pages `memory.stat` counted |
    |---|---|---|
    | 38 967 | 0 | 38 967 |
    | 42 668 | 0 | 42 668 |
    | 39 388 | 184 | 39 572 |

    The sixteenth wave's measurement without the enclosure was 745 of 41 413.
    The diff is the request below. The 1 s session's test
    (`a_drained_reclaim_row_is_credited_whole_inside_one_second_of_pressure`)
    still passes on the reverted tree.
- **`proc`.**
  - `a_count_carries_the_process_after_the_thread` (status) and
    `a_footprint_carries_the_process_after_the_thread` (footprint): `task`
    first, then `proc`. The reference for the namespace is the text of the
    `/proc/self/ns/pid` link, `pid:[<inode>]`, which is a different read from
    the `stat` that `proc_of` does. The pid is the last `NSpid`.
  - A session joining `proc.threads` to `mem.rss` `via: proc` on a launched
    `sleep 2`: 6 matched, 0 unmatched, every row keyed
    `proc:4026531836+<pid>`. Before this change, that join was refused by name.
- **The namespace.**
  - Tests added or updated: `event` (`an_interface_named_without_its_index_...`
    updated, `another_namespaces_interface_of_the_same_index_is_another_interface`
    added), `join::another_namespaces_interface_of_the_same_index_is_not_credited_to_this_ones`,
    `collect::net::qdisc::a_discipline_in_another_namespace_is_keyed_in_that_namespace`,
    `bpf::net_qdisc::an_interface_target_is_compared_with_its_namespace`, and
    the decode round trip with the namespace.
  - The integration files that read `iface` keys still pass:
    `join_engine`, `family_net` and `scenario_net`, run after `make`.
  - **The brief's ground truth, run by hand.** A sender in `unshare -rn` put
    `tc qdisc replace dev lo root netem limit 100000` on its own lo (no
    privilege needed: `sch_netem` was already loaded) and sent 2000 UDP
    datagrams over it. `tc -s` there counted `Sent ... 2000 pkt`.
    `examples/join-iface-qdisc.yml` ran in this namespace, `net:[4026531833]`:
    - as shipped (`net.qdisc --iface lo`): 0 rows, 0 credited. The kernel
      dropped the other namespace's lo by its namespace.
    - with `net.qdisc` on `--system`: 2000 uncredited, 0 matched, and the
      series is `iface:lo@netns:4026533137`, the inode the sender printed for
      its namespace.

    Before this change, the sixteenth wave measured all 2000 credited to this
    namespace's `iface:lo`. That old number is quoted from the filing; I did
    not rebuild the old tree to run it again.

**Reasoned, not shown:**

- **Concurrent stall rows.** The race on `last_ns` is argued above, not
  measured.
- **Namespace reads.**
  - A kernel without `CONFIG_NET_NS` has not been run.
  - The `possible_net_t` offset of 0 is read off `pahole` on this kernel
    (`possible_net_t { struct net *net; /* 0 8 */ }`), not checked by the
    loader.
  - `ns_common.inum` is looked up through BTF (offset 88 here, inside
    `net.ns` at 192).
- **`proc` for a launched target.** It is read at attach, before the target
  execs. That is right because exec keeps the pid namespace. A target that
  `unshare`s a new pid namespace for its children does not change its own.

## What the commit-time run said

One `make check` at `94aab32`, with capabilities. It was red, and stopped at
the first failing test binary:

- `recorder::two_bursts_write_exactly_two_documents_each_holding_its_burst`:
  the fired value was 256 where 303 was expected;
- `budget::the_extra_cpu_a_faster_poll_costs_...`.

Then one `cargo test --workspace --no-fail-fast`: 2276 passed, 6 failed and
10 ignored, summed over the binaries' result lines. Nine tests panicked,
listed below.

- **`scenario_proc::the_only_key_proc_fd_declares_is_the_only_one_its_events_carry`:
  mine, and expected.** The test asks `--keys task` and asserts exactly one
  key per event. `proc.fd` now declares and carries `task, proc`. The test
  file is not this stream's. The diff is filed below, and I applied it,
  watched it pass, and reverted it.
- **Not this stream's, each rerun alone once, and each passed:**
  - `budget::the_extra_cpu_a_faster_poll_costs_...` and
    `budget::a_preempted_hook_call_is_counted_apart_...` (both in the flaky
    tally);
  - `scenario_io::a_call_whose_exit_the_kernel_skipped_is_counted_as_lost`
    and `scenario_io::a_pipe_wait_is_in_the_syscall_layer_...`;
  - `scenario_sched::sharing_one_file_is_the_only_difference_...` and
    `scenario_sched::a_blocking_stack_names_the_call_that_blocked`;
  - `triage::a_join_from_a_held_source_counts_what_fell_outside_its_window`
    and `triage::a_verdict_read_off_a_lossy_profile_...`.

  The load average was 27 to 30 on 24 cpus while they ran.
- **`recorder::two_bursts_write_exactly_two_documents_each_holding_its_burst`
  failed 2 times in 4 runs** (the `make check`, then the first run alone) and
  passed in the other two, including the no-fail-fast run. It is not in the
  flaky tally. The reading that fired was taken while the workload was still
  opening its 300 descriptors, at 256 open. I reason it is the load and not
  this branch, without having shown it on `main`:
  - 256 is where the kernel's descriptor table grows, and that growth can
    wait for an RCU grace period, which a machine at load 30 makes long. So
    the opener stalls there and a 50 ms poll lands inside the stall.
  - This branch changes only which keys a `proc.fd` event carries, not when
    the count is read or what it is.

  I am filing it as a flaky candidate with the 256, not fixing it.

## Could not do, and why

- **The 100 ms session's `enclosing:` line, and its assertion.** They go in
  `examples/cross-machine-reclaim-io.yml` and `tests/cross_machine.rs`, and
  this wave's table gives neither file to this stream. The filing says the
  line must land with the collector change, never before it. Both are filed
  below as one diff, measured above.
- **A test file for the cross-namespace ground truth.** `tests/join_engine.rs`
  is not this stream's. The run above was done by hand, and the test is filed
  below.
- **The eBPF `proc` producer.** No program reads the task's pid namespace.
  `docs/keys.md` says so: "No eBPF source writes it yet".

## Privileged runs wanted

None that need more than `make`. The reclaim crossing and the namespace run
above only needed `CAP_BPF` and an unprivileged user namespace. For the
coordinator's merge run, with capabilities:

```console
cd <merge checkout> && make
cargo test --test cross_machine a_memory_limit_turns -- --nocapture
cargo test --test cross_machine a_drained_reclaim_row
cargo test --lib bpf::net_qdisc bpf::mem_stall
```

## For DECISIONS.md

- **The network namespace beside an interface's index** (moves from "Open"
  to the settled table, replacing "What an interface is"):
  - **What an interface is.** Its namespace's inode and its index there
    (`iface#<netns>:<index>`).
  - **Which namespace each producer writes.** A producer that reads in this
    process's namespace writes this process's. `net.qdisc` writes the
    device's own, `dev->nd_net.net->ns.inum`, read in the kernel.
  - **How another namespace's interface shows.** It is another key. A join
    counts it unmatched or uncredited and prints it
    `iface:<name>@netns:<inode>`. It is never credited to this namespace's.
  - **Why.** An index is per namespace and `net.qdisc` reads every namespace:
    2000 of 2000 packets on another namespace's lo were credited to this
    one's, and are now 2000 uncredited and keyed by their namespace.
- **`proc` has producers.** The per-process procfs sources carry it after
  `task`, read once at attach. This replaces "Stays as vocabulary, with no
  producer". The reason: a join on one process across containers is now
  served, and an exec keeps the namespace.
- **A drained row's enclosing window is first episode to last end.** The
  plain store of the last end is accepted, and a `count` run's rows carry no
  window. The reason: the 100 ms session credits 99.5 to 100 % of the cgroup's
  pages, against 2 % without the enclosure.

## Needs from another stream

### To whoever owns `examples/cross-machine-reclaim-io.yml` and `tests/cross_machine.rs` (the coordinator): the 100 ms session credits its rows

This can land now that `enclosing_ns` is on every drained row. Measured
above:

```diff
--- a/examples/cross-machine-reclaim-io.yml
+++ b/examples/cross-machine-reclaim-io.yml
@@ reclaim-per-pressure-interval
     span: total_ns
+    enclosing: enclosing_ns
     stamped: start
--- a/tests/cross_machine.rs
+++ b/tests/cross_machine.rs
@@ fn a_memory_limit_turns_a_cached_read_into_device_reads_and_each_byte_carries_its_rung
+    assert!(
+        under.overlap_in_brackets >= (capped.pgsteal / 2) as f64,
+        "half the cgroup's pgsteal credited in the program's intervals: {:.0} of {}",
+        under.overlap_in_brackets,
+        capped.pgsteal
+    );
     // A drained bucket placed by its enclosing window rather than left over
```

The session's comment block ("A drained bucket of several episodes has no
one window to place, so its pages are counted uncredited, not spread") then
wants a sentence for `enclosing:`.

### To whoever owns `tests/join_engine.rs`: another namespace's lo is not this one's

This is the brief's ground truth as a test. Run the script below under
`unshare -rn` while the session runs.
It skips when `unshare -rn` or `tc ... netem` fails, with the machine's reason
first: the user namespace is restricted, or `sch_netem` is not loaded.

```sh
ip link set lo up
tc qdisc replace dev lo root netem limit 100000 || exit 9
readlink /proc/self/ns/net
sleep 1
python3 -c 'import socket
s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM); r=socket.socket(socket.AF_INET,socket.SOCK_DGRAM); r.bind(("127.0.0.1",0))
for _ in range(2000): s.sendto(b"x"*64, r.getsockname())'
```

Assert, against `examples/join-iface-qdisc.yml`:

- with `net.qdisc` on `--system`:
  - `readings.credited == 0`;
  - `readings.uncredited == 2000`, which is what `tc -s` in the namespace
    counted;
  - every `net.qdisc` series key ends in `@netns:<the inode the sender
    printed>`.
- as shipped (`--iface lo`): no `net.qdisc` row at all.

### To whoever owns `tests/scenario_proc.rs` (the coordinator): `proc.fd` carries `task, proc`

**Needed for `make check` to be green at the merge.**
`the_only_key_proc_fd_declares_is_the_only_one_its_events_carry` asks
`--keys task` and asserts one key per event. The entry now declares
`task, proc` and every event carries both. I applied this diff, watched the
test pass, and reverted it:

```diff
@@ fn the_only_key_proc_fd_declares_is_the_only_one_its_events_carry
         "--keys",
-        "task",
+        "task,proc",
@@
-        assert_eq!(keys.len(), 1, "one key per event, and it is task: {event}");
+        assert_eq!(keys.len(), 2, "two keys per event, task and proc: {event}");
         assert!(
-            keys[0].get("task").is_some(),
-            "the key on the event is not `task`: {event}"
+            keys[0].get("task").is_some() && keys[1].get("proc").is_some(),
+            "the keys on the event are not `task` then `proc`: {event}"
         );
```

The test's doc comment ("The entry now declares `task` alone") wants
"`task` and `proc`". The refusal half (`--keys task,file`) is unchanged and
still passes: the hint now reads "carries task, proc", which still contains
"carries task" and not "carries task, file".

### To whoever keeps the flaky tally (`docs/open-findings.md`): `recorder::two_bursts_...`

This failed 2 times in 4 at load 27 to 30, firing on a reading of 256 open
descriptors, with 303 expected. The reasoning is under "What the commit-time
run said". The test would hold under load if it asserted that the fired value
is at least the threshold and at most the burst, and kept "the burst's" for
the held readings.

### To whoever owns `src/bpf/btf.rs`: follow a typedef in `member_offset`

`possible_net_t` is `typedef struct { struct net *net; } possible_net_t`.
`net_qdisc::netns_offsets` writes the `net` pointer's offset as 0 because
`member_offset("possible_net_t", "net")` finds only the typedef and skips it.
A `member_offset` that resolves a `TYPEDEF` to its target before it searches
would let the loader check this offset instead of assuming it.

### To whoever owns `src/emit.rs`: nothing required

Series are named by `KeyValue`'s `Display`, so another namespace's interface
is already its own series (`iface:lo@netns:<inode>`). This is listed so the
change is not a surprise.
