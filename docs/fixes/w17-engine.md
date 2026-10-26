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
   - **The plain store is exact.** The row key holds the thread id, and one
     thread is in at most one episode of a stall at a time, so the ends a row
     sees are serial and each store is later than the one before it. This is
     written beside the constant. (The first round said two cpus could race to
     end episodes of one row; the review showed they cannot.)

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
    - `a_counted_row_carries_no_enclosing_window` (review round): a `count`
      run's row, emitted at `stop`, has no `enclosing_ns`. The join's half,
      that a rule naming a missing `enclosing` attribute is refused as
      `MissingSpan`, was already tested in `join`.
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
  - **The brief's ground truth, as a test applied and reverted (review
    round).** The test filed below for `tests/join_engine.rs`,
    `another_namespaces_lo_is_not_credited_to_this_ones`, was appended to the
    file, run once after `make`, and reverted: it passed, with their namespace
    `4026532971`, credited 0, uncredited 2000, 4 rows of theirs, and the
    as-shipped session keeping none. The reviewer separately ran
    `a_queued_packet_on_lo_...` with netem on this namespace's lo: 2000 of
    2000 credited with the in-kernel compare active, so the compare does not
    drop this namespace's own packets.
  - **The brief's ground truth, run by hand** (first round). A sender in `unshare -rn` put
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

- **The last end is exact without an atomic.** Argued from the key (thread id
  in it) and the begin/end pairing (one in-flight entry per thread), not
  measured under contention.
- **The `@netns` label depends on who prints it.** `Display` compares against
  the namespace of the process printing, not the one that recorded. A recording
  rendered from another namespace or machine prints `@netns:<inode>` on every
  interface, the recorder's own included. The identity carries the namespace
  either way, so joins are unaffected, and the label errs towards saying more.
  The comment beside `Display` says so. Fixing it means a recording carrying
  its recorder's namespace, which is `src/encode.rs`'s, and nothing needs it
  yet.
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

### The review round's run

One `make check` on the review round's tree (before its commit), with
capabilities, at a load of 12 to 24 on 24 cpus with other streams running. It
stopped at `family_mem::the_working_set_is_what_was_touched_and_not_what_was_held`.
Then one `cargo test --workspace --no-fail-fast`: 2428 passed, 16 failed,
11 ignored, summed over the binaries' result lines. Each of the 16 was rerun
alone once and **all 16 passed**: `triage` (2), `scenario_sched` (4),
`family_sched` (2), `scenario_io` (2), `unwind` (2), `scenario_runtime`,
`scenario_proc::defect_2_...`, `scenario_net::every_datagram_...`,
`scenario_gpu::the_process_figure_...`. `cargo clippy --all-targets -D
warnings` and `cargo fmt --check` are clean.

- **`family_mem::the_working_set_...`**, not in the flaky tally: after the
  `make check`, it failed 2 of 12 runs alone on this branch, and passed 6 of 6
  on `8725ae0` (a scratch worktree, removed afterwards) at the same load. The
  failures differ: once `mem.wss` had no point because the launched `python3` was gone
  (8 KB resident, exited) by the first read, so `mem.rss` also carried `task`
  alone, its `proc_of` finding no namespace link; once the busiest window read
  191 MB against 278 MB resident, which is the 256 MB allocation inside the
  window. This branch touches `mem.rss`'s keys only, through two read-only
  procfs reads at attach, and not `mem.wss`. I read it as load, but 3 in 13
  against 0 in 6 is not proof, so it is listed here for the merge run.
- **`scenario_proc::the_only_key_proc_fd_...`** passed in the no-fail-fast
  run and failed alone, as expected (finding 1). It passes when no event
  arrives: a 0.1 s `python3` under load can exit before the first 50 ms poll,
  and the loop then asserts nothing. The diff below does not change that; a
  `assert!(!events.is_empty())` would.

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

## Review

The review round (seven findings) and what became of each:

1. **`scenario_proc` is red on the branch alone.** Correct, and known: the
   test file is not this stream's, and the brief says a request to a file a
   stream does not own is filed, not made. The diff is below under "Needs from
   another stream" and **must be applied in the merge commit**, or the merge
   lands red. Unchanged in this round.
2. **Neither ground truth is an automated test.** Both tests live in files this
   stream does not own, so they are still filed rather than landed. What
   changed: the namespace test is now a complete function, applied, run and
   reverted (above), and pasted whole below for `tests/join_engine.rs`. The
   100 ms session's diff was already measured three times. The in-repo guards
   on the engine side are unit tests only: the window computation
   (`collect::mem::stall`), the store of the last end (`bpf::mem_stall`), the
   in-kernel compare's 64-bit register (`bpf::net_qdisc`), and the join's
   refusal to credit another namespace's index (`join`).
3. **The `VALUE_LAST_NS` comment's race.** The reviewer is right: the key
   holds the thread id, so a row's episodes are serial. The comment and the
   handoff now say the plain store is exact, and why.
4. **`Display` uses the printing process's namespace.** True. It is documented
   beside `Display` and under "Reasoned, not shown"; identity is unaffected.
   Not changed.
5. **"Refused" in the brief.** The code counts another namespace's key
   unmatched or uncredited and labels it, which is right for a `--system` run.
   The decision lines below say "counted unmatched, labelled by namespace",
   not "refused". `docs/dev/BRIEF.md` is the coordinator's.
6. **`examples/join-iface-qdisc.yml` and `docs/families/join.md:39`** still
   describe the identity as the index alone. Neither file is this stream's:
   both are filed below.
7. **No test that a `count` run's rows carry no `enclosing_ns`.** Added:
   `collect::mem::stall::a_counted_row_carries_no_enclosing_window`.

## For docs/DECISIONS.md

- **The network namespace beside an interface's index** (moves from "Open"
  to the settled table, replacing "What an interface is"):
  - **What an interface is.** Its namespace's inode and its index there
    (`iface#<netns>:<index>`).
  - **Which namespace each producer writes.** A producer that reads in this
    process's namespace writes this process's. `net.qdisc` writes the
    device's own, `dev->nd_net.net->ns.inum`, read in the kernel.
  - **How another namespace's interface shows.** It is another key: counted
    unmatched or uncredited, labelled by its namespace
    (`iface:<name>@netns:<inode>`), never credited to this namespace's. It is
    not refused: a `--system` run legitimately sees every namespace's
    devices, and the brief's "refused with its reason" reads as "never
    credited silently".
  - **Why.** An index is per namespace and `net.qdisc` reads every namespace:
    2000 of 2000 packets on another namespace's lo were credited to this
    one's, and are now 2000 uncredited and keyed by their namespace.
- **`proc` has producers.** The per-process procfs sources carry it after
  `task`, read once at attach. This replaces "Stays as vocabulary, with no
  producer". The reason: a join on one process across containers is now
  served, and an exec keeps the namespace.
- **A drained row's enclosing window is first episode to last end.** The
  plain store of the last end is accepted, and a `count` run's rows carry no
  window. The plain store is exact because a row's thread ends its episodes
  one at a time. The reason: the 100 ms session credits 99.5 to 100 % of the
  cgroup's pages, against 2 % without the enclosure.

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

This is the brief's ground truth as a test. Append it to the file as it is: it
uses only the file's own `BINARY`, `machine_cannot_serve`,
`machine_s_reason` and `number`. Applied, run once after `make` and reverted
in the review round: it passed (credited 0, uncredited 2000). It skips, with
the machine's reason, when `unshare -rn` or `tc ... netem` fails in the
sender's namespace (the user namespace restricted, or `sch_netem` not loaded),
and it needs `python3` there.

```rust
/// The script a sender runs inside `unshare -rn`: a discipline that queues on
/// its own lo, the namespace's inode on the first line, then 2000 datagrams
/// over that lo once the session has taken its first readings.
const OTHER_NAMESPACE_SENDER: &str = r#"
ip link set lo up || exit 9
tc qdisc replace dev lo root netem limit 100000 || exit 9
readlink /proc/self/ns/net
sleep 1
python3 -c 'import socket
s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM); r=socket.socket(socket.AF_INET,socket.SOCK_DGRAM); r.bind(("127.0.0.1",0))
for _ in range(2000): s.sendto(b"x"*64, r.getsockname())' || exit 9
tc -s qdisc show dev lo
"#;

/// Runs `session` while a sender in another network namespace queues 2000
/// packets on that namespace's lo. `None` when the machine cannot make such a
/// namespace, with the reason printed; otherwise the session's JSON and the
/// inode of the sender's namespace.
fn run_beside_another_namespaces_lo(session: &str) -> Option<(serde_json::Value, u64)> {
    let run = Command::new(BINARY)
        .args(["-f", session, "--format", "json"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("omn should run");
    let sender = Command::new("unshare")
        .args(["-rn", "sh", "-c", OTHER_NAMESPACE_SENDER])
        .output();
    let output = run.wait_with_output().expect("omn should finish");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let sender = match sender {
        Ok(sender) if sender.status.success() => sender,
        Ok(sender) => {
            eprintln!(
                "SKIPPED: this machine cannot queue packets on lo in an unprivileged network \
                 namespace (`unshare -rn` with `tc qdisc ... netem`; the user namespace may be \
                 restricted, or `sch_netem` not loaded): {}. It would have checked that \
                 another namespace's lo is not this one's",
                String::from_utf8_lossy(&sender.stderr).trim()
            );
            return None;
        }
        Err(error) => {
            eprintln!(
                "SKIPPED: `unshare` could not run: {error}. It would have checked that another \
                 namespace's lo is not this one's"
            );
            return None;
        }
    };
    if !output.status.success() {
        assert!(
            machine_cannot_serve(&stderr),
            "the session failed for a reason that is not the machine's:\n{stderr}"
        );
        eprintln!(
            "SKIPPED: {}. It would have checked that another namespace's lo is not this one's",
            machine_s_reason(&stderr)
        );
        return None;
    }
    let said = String::from_utf8_lossy(&sender.stdout);
    let inode: u64 = said
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("net:["))
        .and_then(|rest| rest.strip_suffix(']'))
        .and_then(|inode| inode.parse().ok())
        .unwrap_or_else(|| panic!("the sender names its namespace first: {said}"));
    assert!(
        said.contains("Sent ") && said.contains(" 2000 pkt"),
        "the discipline in the sender's namespace counted its 2000 packets: {said}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    Some((
        serde_json::from_str(&stdout).expect("a JSON document"),
        inode,
    ))
}

/// An interface index is per network namespace, and every namespace has a lo
/// of index 1. A sender in an unprivileged namespace (`unshare -rn`) queues
/// 2000 packets on its own lo while this one's lo is read.
///
/// With `net.qdisc` on the whole machine, the discipline sees the 2000 packets
/// and keys them by the device's own namespace: none is credited to this
/// namespace's lo, all 2000 are counted uncredited, and every series of them
/// says whose lo it is (`iface:lo@netns:<inode>`). Before the namespace was in
/// the key, all 2000 were credited here. And the session as shipped, which
/// targets lo, has the kernel drop them by their namespace: no row at all.
#[test]
fn another_namespaces_lo_is_not_credited_to_this_ones() {
    let shipped = std::fs::read_to_string("examples/join-iface-qdisc.yml").expect("the session");
    let narrowed = "    source: net.qdisc\n    target: {iface: lo}\n";
    assert!(
        shipped.contains(narrowed),
        "the session targets lo for net.qdisc"
    );
    let whole = shipped.replace(narrowed, "    source: net.qdisc\n    target: system\n");
    let session =
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("join-iface-qdisc-system.yml");
    std::fs::write(&session, whole).expect("a session file");

    let Some((document, inode)) = run_beside_another_namespaces_lo(session.to_str().unwrap())
    else {
        return;
    };
    let join = document["joins"]
        .as_array()
        .and_then(|joins| {
            joins
                .iter()
                .find(|join| join["rule"] == "queued-per-link-reading")
        })
        .expect("the session's join");
    let credited = number(&join["readings"]["credited"]);
    let uncredited = number(&join["readings"]["uncredited"]);
    let theirs = format!("iface:lo@netns:{inode}");
    let queued: Vec<String> = document["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|result| result["label"] == "queued")
        .flat_map(|result| result["outputs"].as_array().into_iter().flatten())
        .filter(|output| output["emit"] == "raw")
        .flat_map(|output| output["events"].as_array().into_iter().flatten())
        .filter_map(|event| {
            let attrs = event["attrs"].as_array()?;
            let netns = attrs.iter().find(|attr| attr["name"] == "netns")?;
            Some(netns["value"].to_string())
        })
        .collect();
    eprintln!(
        "their namespace {inode}: credited {credited}, uncredited {uncredited}, {} rows of theirs",
        queued
            .iter()
            .filter(|netns| **netns == inode.to_string())
            .count()
    );
    assert_eq!(
        credited, 0.0,
        "a packet on another namespace's lo was credited to this one's: {join}"
    );
    assert!(
        uncredited >= 2000.0,
        "the 2000 packets their discipline counted are counted uncredited here: {join}"
    );
    assert!(
        join["top"]
            .as_array()
            .into_iter()
            .flatten()
            .all(|row| row["key"] != "iface:lo"),
        "no row of their lo is labelled as this one's: {join}"
    );
    assert!(
        join.to_string().contains(&theirs)
            || queued.iter().any(|netns| *netns == inode.to_string()),
        "their rows carry their namespace, {theirs}: {join}"
    );

    // As shipped, the target is this namespace's lo, and the kernel compares
    // the namespace: none of theirs is a row.
    let Some((document, _)) = run_beside_another_namespaces_lo("examples/join-iface-qdisc.yml")
    else {
        return;
    };
    let join = document["joins"]
        .as_array()
        .and_then(|joins| {
            joins
                .iter()
                .find(|join| join["rule"] == "queued-per-link-reading")
        })
        .expect("the session's join");
    assert_eq!(
        number(&join["readings"]["credited"]) + number(&join["readings"]["uncredited"]),
        0.0,
        "a target of this namespace's lo kept a packet of another namespace's: {join}"
    );
}
```

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

### To whoever owns `examples/join-iface-qdisc.yml`: the header names the namespace

The header says a join compares `iface` "by the kernel's index". It is now the
namespace and the index:

```diff
-# of counter (`iface:lo/drops`). A join compares an `iface` key by the kernel's
-# index -- resolved from the name at the read when a producer has only the
-# name, and a name's sub-series after `/` is not part of the interface -- so
-# the two meet on lo, and on nothing else (`docs/keys.md`).
+# of counter (`iface:lo/drops`). A join compares an `iface` key by its network
+# namespace and the kernel's index there -- the index resolved from the name at
+# the read when a producer has only the name, and a name's sub-series after `/`
+# is not part of the interface -- so the two meet on this namespace's lo, and
+# on nothing else; another namespace's lo is another key (`docs/keys.md`).
```

### To whoever owns `docs/families/join.md`: the identity is `iface#<netns>:<index>`

Line 39 gives the identity as `iface#<index>`. After this wave it is
`iface#<netns>:<index>` (`iface:<netns>:<name>` when the index stayed 0); a
sentence after the paragraph could say why: an index is per namespace, and
`net.qdisc` on the whole machine keyed 2000 packets on another namespace's lo
as this one's before the namespace was in the key.
