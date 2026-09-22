# The shared layer — what the sixth wave asked for, and what became of each

Stream S of the seventh wave, taken by the coordinator. The sixth wave's five
family streams wrote their requests under `## Needs from another stream` in
`docs/open-findings.md` rather than reaching into files they did not own, which
was right and left roughly twenty of them unlanded. Three were asked for twice
by two families independently, which is the whole argument for a stream that
lands them once: the second wave learned it and the third paid for not doing it.

This document is the counterpart to `docs/families/<family>.md` — what was
landed, what turned out to be something else, and what was left with the reason.

---

## Landed

### `Btf::member_offset` descends anonymous structs and unions — asked by `io` and by `net`

`folio.mapping`, `sk_buff.protocol`, `sk_buff.skb_iif`, `sock_common.skc_daddr`
and `skc_dport` are all declared inside anonymous members, and a lookup that
walked only the direct members reported every one of them absent from a kernel
that has them. Three collectors shipped around it: `net.drops` carries no
interface, `net.tcp.lifetime` reports the local endpoint only, and
`io.pagecache` has no collector at all because its only argument is a `folio *`.

What landed is what C does with the same declaration: when a member has no name
and its type is a struct or a union, search inside it and add the enclosing
offset. Direct members are searched first at every level — C forbids the
ambiguity that would make the order observable, so the rule only matters for a
malformed blob, where finding the named member is the better answer. A pointer
to a struct is **not** descended into: `folio.mapping` is an `address_space *`
and `address_space.host` is not a member of `folio`, so following it would
answer an offset in the wrong object. There is a test for that, because it is
the mistake a helpful implementation would make.

Checked against the running kernel rather than only against a hand-built blob.
The kernel corrected one assumption on the way: `sock_common.skc_daddr` is at
offset **zero**, being the first member of the union it lives in, so an
"overlaid fields are not at zero" assertion would have been asserting this
kernel's layout rather than the lookup. What the test asserts instead is that
each of the four resolves and that two of them are not the same field.

### `Btf::enum_variant` — asked by `io` and by `net`

`net.drops` reported `enum skb_drop_reason` as a bare number and `io.writeback`
`enum wb_reason` the same way, both because a table of names compiled into `omn`
would be a property of the kernel this was built against rather than the one
running: this enum has gained members in each of the last several releases, and
`wb_reason` has lost one. A mislabelled drop reason does not read as wrong, it
reads as a different fault.

The kernel publishes the pairs in the same BTF everything else here comes from.
`enum_variant(name, value)` reads them, handles `ENUM` and `ENUM64` and their
signedness, and answers `None` for a value this kernel does not declare — which
is the point: a number with no name is better than a wrong name.

This one corrected an assumption too, and it is worth keeping. The test was
written expecting value zero of `skb_drop_reason` to be
`SKB_DROP_REASON_NOT_SPECIFIED`. On this kernel it is `SKB_NOT_DROPPED_YET`.
The guess was a name, it was wrong, and it would have been wrong silently in
every rendered drop table — which is the argument for reading the kernel's own
table in one line, made by the change's own test before it was even used.

### The helper numbers, the missing instruction, and five copies of three sequences

`insn::helper` now names `GET_CURRENT_COMM` (16) and `PROBE_READ_KERNEL_STR`
(115), which `io`, `net`, `proc` and `sched` had each declared locally — the
last three in a module called `extra_helper` with the reason written down, which
is the honest form of a workaround and is now gone.

`Insn::and64_imm` exists. `sched.futex` masked two flag bits off a syscall
argument by conditional subtraction because there was no bitwise `and`:
correct, tested, and eight instructions where two do.

And the sequences, which is the part that mattered:

| Sequence | Copies before | Copies now |
|---|---|---|
| `read_field` | 8 | 1 |
| `count_drop` | 6, in two shapes | 1 |
| `push_log2_bucket` | 5 | 1, plus two label sets |
| `bucket_low_ns` | 4 | 1 |

`push_log2_bucket` is why this was urgent rather than tidy: it decides which
bucket every kernel-built histogram in this repository puts a measurement in,
and two copies disagreeing by one would report every duration in the
neighbouring bucket — a factor of two — with nothing anywhere saying so. Four
copies of its test existed, one beside each copy of the sequence; one sequence
needs one test, and that is now where it is.

The labels stay with the caller, because a label is defined once per instruction
stream: `net_program.rs` and `mem_program.rs` keep a `Log2Labels` const and a
two-line wrapper, so no `net` or `mem` program had to change. `sched_common.rs`
re-exports the shared names for the same reason. There is a new test that two
searches in one program keep their labels apart, which is the failure the
caller-supplied labels exist to prevent.

### The capability advice is on map creation, where the failure actually is

Defect 8 of `docs/scenarios/proc.md`. Every program in this layer attached
`permission_advice` to its `prog_load`, and a program load is not the first
privileged thing a collector does — **map creation is**, and it fails first. So
what somebody without `CAP_BPF` saw was

```text
cannot create the `sched_process_exec` ring buffer: Operation not permitted (os error 1)
```

which names neither the capability nor the way to get it. `map::map_failure` now
carries the advice, and **51 creation sites across 18 files** were moved onto
it, along with the two local copies `proc` had written. Nobody noticed for four
waves because the message only appears on a machine without the capabilities,
which is not the machine any of this was written on — so the two tests for it
construct the refusal rather than waiting for a machine that produces one.

### `attach::tracefs_root` and `attach::tracepoint_layout`

`attach.rs` knew where tracefs may be mounted and how to read a tracepoint's
numeric id, and published neither the root nor a way to read the `format` file
beside it. `gpu.job` is the only collector here on the classic tracepoint path
and needs the field layout, so `sched_drm.rs` carried a second copy of the two
mount paths and a `format` parser of its own. The parser moved with its two
kernels' fixtures as the request said it could, `sched_drm.rs` keeps its names
through a re-export, and whoever takes the next classic tracepoint — `io.fs` and
`io.readahead` will, if they take that path — finds it published.

### The catalog pass

Each of these was filed by the family that met it, and each is a promise a
reader could ask for and not get.

- **`io.readahead`'s glob became two names.** `tracepoint:filemap:*` is not a
  name: no probe covers it, the source reported `?`, and the chain fell through
  to `bpf:fentry`, which this layer does not have — unreachable from both ends.
  It now requires `mm_filemap_get_pages` and `mm_filemap_add_to_page_cache`,
  which are the denominator and the numerator of readahead effectiveness, and
  the reasoning for choosing those two of the subsystem's five is stream A's,
  recorded in `docs/open-findings.md`. **This is the one entry whose change
  moved the numbers**: 86 usable here became 87, and 10 unverified became 9.
- **`mem.reclaim` requires the memcg pair** as well as the global one. A cgroup
  that hits its own `memory.max` reclaims through
  `mm_vmscan_memcg_reclaim_begin/end`, and the entry offers a cgroup target, so
  the requirement now covers the tracepoints a cgroup request actually needs.
  The collector already attaches both.
- **`mem.compaction` requires its ending.** `unit: ns`, and the chain required
  `mm_compaction_begin` alone; a duration needs both ends, and a kernel with one
  and not the other was called available and refused three layers later.
- **`net.qdisc` requires `qdisc_enqueue`** as well as the dequeue — a wait is
  the gap between the two — and its key became `iface` rather than `sock`,
  because a row aggregated over packets has an interface and no socket.
- **Four sets of keys and targets nothing could carry** were dropped:
  `mem` from `mem.page_alloc` and `mem.kmalloc` (the identity of one allocation
  is exactly what counting in the kernel discards), `req` and `file` from
  `proc.syscall`, `cgroup` from `proc.exec` and `proc.exit`, and the `pid`,
  `cgroup` and `container` targets plus the `daddr`/`dport` filter fields from
  `net.tcp.lifetime` and `net.tcp.retransmit`.
- **`sched.irq`'s summary stopped promising the soft half**, which this build
  does not attach to. Stream E asked for the requirement to gain
  `softirq_entry/exit` so that a collector *may* read them; requiring
  tracepoints nothing attaches to is the same over-promise in the other
  direction, so what landed is the entry saying what it measures. The soft half
  is collector work and is named under **Left** below.

Two of those key drops moved a refusal a layer earlier, and both were caught by
scenarios that had asserted the old answer:

```
-  assert_eq!(code(&keyed), EXIT_NO_COLLECTOR)     # the collector refused at attach
+  assert_eq!(code(&keyed), EXIT_REJECTED)         # the validator refuses before anything is attached
```

That movement is the improvement, not a regression to paper over: an invalid
combination is refused with the reason, and the earliest layer that can see it
is the one that should. The `net` scenario needed more than a code change — its
third case was *"a target the catalog offers and this build cannot serve"*, and
that case no longer exists for `net.tcp.lifetime`, so it now uses `net.socket`,
which resolves onto `bpf:fentry` and has no collector.

And one shipped session stopped validating, which is the same event seen from
the user's side: `examples/web-latency.yml` asked `net.tcp.lifetime` for one
process, for a `req` key and for a `dport` filter, and could never have run. It
now asks for what the source serves, with the reason in the file.

---

## Something else

**`direction` on `mem.page_alloc` and `mem.kmalloc` is not a catalog line.** The
`mem` stream offered two ways to close it: declare `direction` a filter field,
or split each source in two. Neither is right today, and the reason is in the
collector rather than in the entry — **the frees are not events at all**. They
are counted in the kernel, drained, and reported in a note:

```
N page frees totalling M pages happened in the same window and are not events
here: a series is grouped by key, `direction` is neither a key nor a filter
field, and a histogram of allocations and frees together is a histogram of
neither
```

Declaring a filter field over an attribute no event carries is exactly the
defect the rest of this pass removed, in a new place. Publishing the frees is
the real change, and it is collector work in `src/collect/mem/**` plus a
decision about what the default emit then means — a `hist` that merges
allocations and frees is a histogram of neither, and that is what publishing
them without a split would produce.

---

## Left, with the reason

- **`daddr` and `dport` on the `net` connection sources.** The half that was
  mechanism has landed: the BTF walk reaches `skc_daddr` and `skc_dport`, and a
  test says so against the running kernel. The half that is left is the
  collectors reading them — a longer record, two more stack slots and their
  parsing in `net_lifetime.rs` and `net_retransmit.rs` — which is `net` family
  work rather than shared layer. Until then the entries do not offer the fields,
  which is why they were dropped rather than left standing.
- **`net.drops`' drop reason as a name.** Same shape: `enum_variant` exists and
  is tested against `skb_drop_reason` on this kernel; the collector still
  reports the number.
- **`io.pagecache` and `io.readahead`.** Both are now unblocked —
  `folio.mapping` is reachable and the glob is two names — and both need a
  collector written, which is `io` family work.
- **A cgroup target for `sched` and `proc`.** Ten entries declare
  `cgroup: bpf_only` and every collector refuses by name. The eBPF half is one
  helper (`bpf_get_current_cgroup_id`, 80) or a map
  (`bpf_current_task_under_cgroup` against a `CGROUP_ARRAY`, which is the one
  that answers the ancestor question a container needs), and the other half is
  turning a `Target::Cgroup(path)` into the id, which is not eBPF and belongs
  where `Target::Cgroup` is resolved. A helper added with no caller is a
  signature invented from a description, so it was not added: this is one piece
  of work in two layers, and it should be one stream's.
- **`sched.irq`'s soft half.** `softirq_entry` and `softirq_exit` are in this
  kernel's BTF, checked by name. Attaching to them is a collector change, and
  the entry now says what it measures rather than what it might.
- **A bounded read at an offset the verifier has not seen** — the `__data_loc`
  problem `gpu.job` works around by carrying the scheduler thread's name. It is
  a real piece of eBPF work and nothing in this build is blocked on it yet.
- **`Insn` fragments for `count_update_failure`.** It is generic and only
  `sched` uses it, so it stayed in `sched_common.rs`. A request is landed with
  the caller that wanted it; this one has one caller.
- **A note is not a degradation** (filed by `net` and by `proc`). It is real and
  it belongs to whoever owns `src/emit.rs`, which in this wave is stream E.
- **The block key's third component** (`docs/open-findings.md`, stream A's item
  5). It is a question about `KeyValue::Blk` and the join engine rather than
  about the eBPF layer, and `gpu:fence_ctx+seqno` is the same shape and may want
  the same answer.

---

## What this cost, and one thing it found

`make check` is green: **1379 tests**, up from 1367, on a binary carrying
`CAP_BPF` and `CAP_PERFMON`. The twelve are the moved tests plus the new ones —
seven for the BTF walk and the enum, two for the capability advice on map
creation, one for two log2 searches in one program, one for the `and`
encoding — net of four duplicate bucket tests that became one.

**`docs/scenarios/io.md` says `cargo test --test scenario_io` "was run
repeatedly and never flapped". It flaps.** Measured while this stream ran, with
four other agents on the machine and a load average around 5 on 24 cores: two
failures in three runs of that file alone, on
`a_known_number_of_direct_reads_is_found_one_for_one` and on
`the_tracer_and_the_kernels_own_counter_see_the_same_requests`, both within
about 1 % of their thresholds. The same file on the tree *before* this stream's
changes failed one run in three, and that failure was not a margin at all:

```
0 bytes traced for the 16777216 the workload read
```

So the flakiness is older than this stream and worse than the document records,
and a total miss is a different fact from a 1 % shortfall. It belongs to whoever
owns `io`, with the numbers rather than an impression.
