# The devices -- `io`, `net` and `mem` on the kernels that are not this one

Stream N of the thirteenth wave: the `io`, `net` and `mem` programs read the
shapes 5.15 and 6.6 write, and the requests filed against those collectors.
This is the deliverable and the handoff; `docs/families/io.md`, `net.md` and
`mem.md` keep what the earlier waves measured.

Where a number below says *guest*, it was measured in the portability
matrix's own guests -- Ubuntu's mainline 5.15.221 and 6.6.157 builds, booted
by virtme-ng over the host's filesystem, as root, with a loop-mounted ext4 as
the scratch disk and a delegated cgroup subtree. Where it says *host*, on the
development machine's 7.2.

---

## 1. The widenings

Each is a second shape in the program and in its `raw_tracepoint:`
requirement, in the same commit, and each program has a unit test that holds
its accepted shapes and its catalog line to each other
(`mem_program::assert_catalog_lists`): the probe answers from the line and the
attach from the program, and a shape one of them knows and the other does not
is a source that reads *missing* where it could be served, or *usable* where it
refuses.

The prototypes, read out of the guests' own `/sys/kernel/btf/vmlinux`:

| tracepoint | 7.2 (host) | 6.6 | 5.15 |
|---|---|---|---|
| `mm_vmscan_direct_reclaim_begin` (and `memcg_`) | `(gfp_t, int, mem_cgroup *)` | `(int, gfp_t)` | `(int, gfp_t)` |
| `mm_vmscan_direct_reclaim_end` (and `memcg_`) | `(long unsigned int, mem_cgroup *)` | `(long unsigned int)` | `(long unsigned int)` |
| `mm_compaction_begin` | `(compact_control *, lu, lu, bool)` | same | `(lu, lu, lu, lu, bool)` |
| `mm_compaction_end` | `(compact_control *, lu, lu, bool, int)` | same | `(lu, lu, lu, lu, bool, int)` |
| `kmalloc` | `(lu, void *, size_t, size_t, gfp_t, int)` | same | `(lu, void *, size_t, size_t, gfp_t)` |
| `kmalloc_node` | -- | -- | `(lu, void *, size_t, size_t, gfp_t, int)` |
| `block_rq_complete` | `(request *, blk_status_t, unsigned int)` | same | `(request *, int, unsigned int)` |
| `sock.sk_ino` | a member | absent | absent |

Two things the brief did not list, found by the guests:

- **Reclaim's `end` moved too**, on both LTS kernels, and so did compaction's
  on 5.15. The `begin` programs read no argument at all; the `end` reads
  reclaim's pages in slot zero, which is `long unsigned int` in every shape
  (`what_the_end_program_reads_is_in_the_same_slot_in_every_shape`).
- **5.15 splits `kmalloc` in two.** `__kmalloc_node` and
  `__kmalloc_node_track_caller` -- where a socket buffer's data comes from --
  fire `kmalloc_node`, which 6.1 folded into `kmalloc`. With `kmalloc` alone
  the guest reported **77 584 bytes for 2000 datagrams of 2 KiB**. The program
  now attaches the same instructions to `kmalloc_node` wherever the kernel
  publishes it (`Allocator::companions`), and the scenario passes there.

**`block_rq_complete` on 5.15 carries an errno, not a status.**
`blk_status_to_errno` has already run, so the slot is `-5` where a later
kernel writes `10` for the same failure. The slot is read the same way, and
the event names what it holds: `errno` on 5.15, `status` elsewhere, with a
note saying which (`BlockProgram::status_is_errno`).

**`sock.sk_ino` through `sk_socket`.** `SocketInode` in `net_program.rs`
reads `sock.sk_ino` where the kernel has it, and otherwise what `sock_i_ino`
reads: `sk_socket`, then the `vfs_inode` of the `socket_alloc` that embeds the
`struct socket`, then its `i_ino` -- all named members. Not
`sk_socket->file->f_inode`: a socket's file can be absent while its inode is
not, and the allocation's inode is the one `/proc/net/tcp` prints. The
`btf_member:sock.sk_ino` requirement is gone from the three entries, because
every kernel has the walk. Both shapes are built and audited in every
program's unit tests on the host, which only ever chooses the member.

### The ground truth

In both guests, the scenarios below pass, against references that share
nothing with the programs:

| source | scenario | reference |
|---|---|---|
| `net.tcp.lifetime` | `a_connection_of_a_known_length_and_volume_is_found_by_both_of_its_ends` | the client socket's `socket:[n]` from `/proc/self/fd`, now asserted equal to the event's `inode` -- the walk's own ground truth |
| `net.drops` | `every_datagram_sent_to_a_closed_port_is_a_drop_with_one_reason` | 512 datagrams, `Udp: NoPorts` |
| `net.tcp.retransmit` | `every_segment_the_kernel_resent_is_one_this_run_saw` | the namespace's `RetransSegs` |
| `mem.kmalloc` | `a_known_number_of_megabytes_and_datagrams_is_found_in_the_allocators_ledger` | 2000 datagrams of 2 KiB |
| `mem.reclaim` | `a_cgroup_squeezed_until_it_reclaims_reports_what_the_kernel_counted` | the cgroup's own `pgsteal_direct` (`pgsteal` on 5.15) |
| `mem.compaction` | `compaction_is_seen_where_the_kernel_says_it_happened_and_skipped_loudly_where_it_did_not` | `compact_migrate_scanned`, after writing `compact_memory` |
| `io.latency` | `a_recycled_request_pointer_is_told_apart_by_its_scope` | 67 714 requests, 0 keys used twice (5.15) |
| `io.size` | `the_tracer_and_the_kernels_own_counter_see_the_same_requests` | the device's `/proc/diskstats` |

The host runs every one of them too, with the member shapes.

Three things the guests taught the scenarios, each now in the test:

- **A cgroup's reclaim is not all done by its own tasks.** In the 6.6 guest
  `dd` reclaimed 3838 pages and two `kworker/u8` 2081 and 1856: the loop
  device's workers charge the backing file's pages to the cgroup that issued
  the I/O, and reclaim *for* it from the root cgroup. The three add up to the
  cgroup's `pgsteal_direct`, 7775, exactly. The test now counts the cgroup's
  tasks and the kernel workers reclaiming at cgroup scope; on the host's NVMe
  the second part is empty. The event's `cgroup_id` is the reclaiming task's,
  not the memcg's -- see "Left".
- **5.15's `memory.stat` has no `pgsteal_direct`**, only `pgsteal`, and the
  workload read zero from the missing key. It falls back now, and a memcg has
  no kswapd, so on 5.15 the whole count is the direct one.
- **A process in the root cgroup looped forever** in the workload's walk up
  the hierarchy (`0::/` gave `/sys/fs/cgroup/`, whose trailing slash carried
  the walk past the stop to `/`). `examples/mem-reclaim-squeeze.sh` is a
  companion of `examples/mem-reclaim-squeeze.yml`, used by this family alone,
  and is fixed here; the ownership table lists the `.yml` only, so it is named
  for the coordinator.
- **Compaction is asked for where it can be.** As root the test writes
  `/proc/sys/vm/compact_memory` inside the window, which compacts through the
  same `compact_zone` that raises `mm_compaction_begin`; without root it is the
  passive watch it was. The guests are where it is a check rather than a skip.

**The portability sweep** (`scripts/matrix.sh v5.15.221 v6.6.157 --configs
caps`, run once on the committed tree, c32243f): the judge passes both cells,
0 broken, and every widened source is listed usable and its run attaches
(exit 0) in both -- `mem.reclaim`, `mem.compaction`, `mem.kmalloc`,
`mem.page_alloc`, `io.latency` (system and launch), `net.drops`,
`net.tcp.lifetime`, `net.tcp.retransmit`:

```
5.15.221-caps   101 sources, 76 available, 87 of 140 runs attached, 0 broken
6.6.157-caps    101 sources, 82 available, 97 of 140 runs attached, 0 broken
```

By the coordinator's instruction no further matrix run was started. The
cells `docs/portability.md`'s P1 and P2 name that these touched and that were
not swept here -- **the coordinator's to run alone**, each one where the
sources above should read usable and attach:

```
scripts/matrix.sh v6.1.188 --configs caps     # the net.* inode walk; mem.reclaim
scripts/matrix.sh v5.15.221 v6.6.157 --configs root
```

**What cannot be served on these kernels, and is not a widening.**
`io.pagecache` reads `mm_filemap_get_pages`, which neither 5.15 nor 6.6
defines (and 5.15's `mm_filemap_add_to_page_cache` takes a `page *`): the
lookup the source counts from does not exist there. `net.request` on 5.15 is
P5 (no uprobe-multi), stream P's.

---

## 2. What the collectors said wrong

Each was checked against the code first.

| request | what landed | ground truth |
|---|---|---|
| `3796` net.drops names its reason | `reason` is the kernel's name from BTF (`NO_SOCKET`), `reason_code` the number; a value the enum does not name is its number | 512 datagrams to a closed port are `NO_SOCKET`, on the host and in both guests |
| `3760` the remote endpoint | `net.tcp.lifetime` and `net.tcp.retransmit` read `skc_daddr`, `skc_v6_daddr` and `skc_dport` through the anonymous-member walk; both entries declare `daddr` and `dport` | the lifetime scenario: the client's peer is the listener's port and the server's the client's; unit tests pin the members inside `skc_addrpair` / `skc_portpair` |
| `3972` a page free is an event | every `mem.page_alloc` event carries `direction`, a filter field; a predicate naming it makes the frees events, and without one the frees are counted and the note says how to ask. `mem.kmalloc` cannot: `kfree` has no size, so `direction` is not its filter field and the validator refuses it | unit tests on the predicate and the attribute |
| `2585` net.iface by counter kind | one series per interface and kind, keyed `iface:<name>/<kind>`, value `frames`; `iface` stays the bare name | 1000 datagrams to a down veth: the rate over `veth0/drops` is 1000, over `veth0/errors` 0 |
| `2745` `daddr` without the port | `net.tcp.rtt`'s `saddr`/`daddr` are the address alone, rendered as the other net sources render them | `daddr == '127.0.0.1'` now keeps the connection the port filter kept |
| `2619` mem.swap counted | `count` mode: one event from the first read to the last, carrying the window; `rate` back in `emits` | a 32 MiB cgroup with 192 MiB touched: the counted window holds at least the polled one and the workload's floor |
| `1503` one PSI reader | `src/collect/psi.rs`, used by `io.pressure` and `mem.pressure` | every `/proc/pressure/*` file a source reads, parsed twice, a share in [0, 1] -- and `/proc/pressure/irq` found to write `full` only |
| `5132` flusher progress vs kworker writes | investigated, below | |

**`5132`, investigated.** `io.writeback`'s `pages` is `wb_writeback_work.nr_pages`
spent over a round, which is what the filesystem reported to writeback
control, and only for work that goes through a flusher round. A 96 MiB write
through a 32 MiB cgroup on this btrfs, with `sync` after it, over 5 s on the
loaded machine: 48 rounds reporting 66 953 pages (reason 1 vmscan: 41 438;
reason 2 sync: 25 515; three periodic rounds: 0), beside `io.latency` crediting
1044 MiB of writes to a btrfs worker and the kernel's own `nr_written` moving
by 264 948 pages. So the flusher's budget covered a quarter of what was
written back in that window. Not settled: how much of the rest is other
streams' writes and how much is btrfs submitting through its own workers
without spending the flusher's budget -- a quiet machine and a second
filesystem would split it. What landed is the caveat saying what `pages` is.

**Two things the scenarios found**, each in the catalog now:

- `mem.wss` does not see a working set the TLB holds. The referenced bit is set
  on a page-table walk, and a clear does not evict the TLB's entries: 16 MiB
  in eight huge pages walked twice straight after a clear read as 4 KiB.
  That, not scheduling, is what the old 300 ms sleep in the tally's #23 was
  hiding (a sleep lets the entries age out). The walker now uses small pages,
  which do not fit, and the test waits for two walks the child counts.
- `mem.reclaim`'s `cgroup_id` is the reclaiming task's cgroup. Kernel workers
  that charge to a cgroup reclaim for it from the root one (above). On the
  host kernel `mm_vmscan_memcg_reclaim_end` also carries the `mem_cgroup *`
  being reclaimed, which a later change could read to key the row by the
  memcg; the LTS shapes do not have it.

---

## 3. The catalog's words

- `mem.rss`: up to 3 MiB of a read beside a preload source is the shim's ring
  (65 536 slots of 48 bytes, from `src/collect/mem/heap/mod.rs`), resident as
  records are written.
- The multi-size THP caveat (`4699`) is in `mem.faults`, which is stream P's
  entry this wave: the replacement text is filed. `mem.thp`'s caveat is a fact
  and stays.
- `io.fsync` and `net.tcp.connect`'s attach cost: already in both entries
  (abc6d37), nothing to do.
- New: `io.writeback`'s `pages` (above), `mem.wss`'s TLB, `mem.page_alloc` and
  `mem.kmalloc`'s frees, `net.iface`'s series, `io.latency`'s `errno` note.

---

## 4. The tally's rows in these files

| # | test | what changed |
|---|---|---|
| 16 | `scenario_net::every_segment_the_kernel_resent_is_one_this_run_saw` | `RetransSegs` is read after the connection is aborted (`SO_LINGER` 0 stops its timer); the connection is matched by both ports |
| 17 | `scenario_net::a_connection_is_joined_from_its_handshake_to_its_close_by_the_cookie_alone` | matched by (client port, listener port), which the peer's end on `net.tcp.lifetime` makes possible |
| 18 | `scenario_io::the_page_cache_sessions_say_what_their_headers_predict` | exact when the run reports no loss, and bounded -- never above 4096 misses, a warm window negative and no lower than the file -- when it reports any: a lost record is a whole window of pages, not one (measured in this wave's workspace run: 197 of 214 readahead records lost to a full ring, a warm window of -3584). Not taken: a `mincore` before the warm read -- the workload is a Python one-liner -- so an eviction between the reads is still not visible |
| 19 | `scenario_io::the_tracer_and_the_kernels_own_counter_see_the_same_requests` | class (d): the failure message now carries the traced requests by task, which is what would decide it |
| 20 | `scenario_mem::every_mode_and_emit_of_the_sampled_sources_...` | stream R's `src/emit.rs` line; filed |
| 21 | `scenario_mem::both_huge_page_backends_read_the_same_promotion_counter` | `/proc/meminfo` is read around every round, and the pair asserted on is bracketed by the reads around its own round |
| 23 | `collect::mem::wss::tests::a_process_that_holds_much_and_touches_little_reports_the_little` | the walker counts its walks; the test waits for two after the clear, on small pages (above); the resident set is compared with the child's own `VmRSS` |

---

Also found in this wave's workspace run, and fixed:
`scenario_net::every_packet_through_a_discipline_is_in_a_row_or_a_counted_loss_on_a_short_tick`
counted 708 253 dequeues for 632 000 datagrams -- another checkout's suite made
an `omnq0` in its own namespace at the same moment, and a raw tracepoint sees
every namespace. The device is named after the test's pid now.

## Needs from another stream

Filed in `docs/open-findings.md` under "the thirteenth wave, stream N":
`pub mod psi;` for R; `net.softirq`'s `facts()` at R's merge; the tally's #20
for R; P's `validate.rs` test that used `mem.swap` as its broken promise; the
`mem.faults` THP caveat for P and `DECISIONS.md`; `topk` back into
`net.bytes` after R's merge, for the coordinator.

For the coordinator, not filed there:

- `examples/mem-reclaim-squeeze.sh` is edited here (the walk that looped
  forever from the root cgroup; `pgsteal` where 5.15 has no
  `pgsteal_direct`). The table lists `examples/mem-*.yml`; this is that
  session's workload and nothing else runs it.
- `docs/scenarios/net.md`: defects 5 (`net.iface` answers about errors alone)
  and 6 (`daddr ==` can never match) are fixed.
- `docs/portability.md` P1 and P2: every entry naming `mem.reclaim`,
  `io.latency`, `mem.kmalloc`, `mem.compaction`, `net.drops`,
  `net.tcp.lifetime` or `net.tcp.retransmit` is closed by the sweep above;
  `proc.exit` and `net.request` are not this stream's.
- `CATALOG.md`: no new notation.

## Index rows

No session was added.

## For `DECISIONS.md`

- **A program that reads the same slots out of several prototypes lists them
  all, and its catalog line lists the same shapes in the same order**, held
  together by a test per program (`mem_program::assert_catalog_lists`). A
  kernel that splits a tracepoint the program reads (5.15's `kmalloc_node`)
  is attached to as well, where it exists, rather than served short.
- **A slot that means something else on another kernel is named for what it
  holds** (`errno` on 5.15's `block_rq_complete`, `status` elsewhere), rather
  than translated through a table no run could check.
- **A counter kind that is a different failure is a different series**, keyed
  in the key's own name where the entry has one key (`iface:veth0/drops`).

## The suite

One `make check` on the final tree, then, because it went red, one `cargo test
--workspace --no-fail-fast`. Red, and each accounted for:

- `validate::tests::an_emit_no_declared_mode_can_produce_is_a_broken_promise`
  (stream P's `src/validate.rs`): expected, it uses `mem.swap` as the entry that
  cannot produce `rate`, which `count` now serves; the one-line change is filed.
- `collect::tests::weighing_a_run_costs_a_fraction_of_the_run` (the tally's
  #12) and `collect::tests::an_escalated_layer_over_its_budget_...` (in the
  tally): passed alone.
- `scenario_sched::sharing_one_file_is_the_only_difference_between_these_two_runs`
  (stream S's): passed alone.
- `scenario_io::the_page_cache_sessions_say_what_their_headers_predict` and
  `scenario_net::every_packet_through_a_discipline_...`: this stream's, fixed as
  above and passing.
- `scenario_mem::a_gibibyte_of_reads_costs_one_l1_miss_per_cache_line`, once
  in an earlier run of the file: passed alone; filed for the tally.

`tests/scenario_{io,net,mem}.rs` and `tests/family_{io,net,mem}.rs` pass whole
on the host with the capabilities on the binary. The guests ran the eight
scenarios of section 1 as root: all pass in both, alone; beside each other in
one 2 GiB guest `a_recycled_request_pointer_is_told_apart_by_its_scope` was
killed by the guest's OOM killer (the loop disk lives in guest memory).
