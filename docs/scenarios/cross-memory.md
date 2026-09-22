# The memory crossing — which allocation cost memory, and did it stay

Stream P1 of the seventh wave. The question is one no single source answers:
follow an allocation from the call that made it to the resident pages it still
holds at the end. Four sources answer it between them, and **they do not answer
it with the same grade of evidence**, which is most of what this document is
about.

| | |
|---|---|
| Sessions | `examples/cross-memory-which-allocation.yml`, `examples/cross-memory-did-it-stay.yml` |
| Tests | `tests/cross_memory.rs` — eight, five of them against the workload's own numbers and three pinning defects |
| Sources | `mem.heap.alloc`, `mem.faults`, `mem.rss`, `mem.heap.retained` — all already checked against ground truth by `docs/scenarios/mem.md` |
| New mechanism | none. What is new is the arithmetic, and the honesty about what it rests on |

---

## What this crossing claims, and what it does not

**It claims three things, each at the evidence level written beside it.**

1. **Which allocation the kernel had to make real — `KEY`.** A fault carries the
   address that faulted; an allocation carries the range it owns; both are
   `mem:[base,len)`, and the engine's `interval` join says `evidence KEY`. This
   is the only claim here that is about one allocation.
2. **What a free returned — `KEY`, computed outside the engine.**
   `mem.heap.retained` intersects the ranges the shim saw freed with the pages
   `/proc/[pid]/pagemap` says are still present. That is an address inside a
   range, which is a key — but it happens inside one collector and reaches the
   output as a total, so no join line carries it.
3. **That the resident size moved by what each phase wrote — `TEMPORAL`.** A
   reading of `mem.rss` is a total for the process, taken at an instant. No key
   on it names an allocation, and no rule in this engine can attach one. What
   the timeline shows is that residency rose by 32 MiB **in the interval** four
   8 MiB ranges opened in. That is *consistent* with them. It is not a
   measurement of what any one of them costs, and this document never says it
   is.

**It does not claim how many resident bytes a given live allocation holds at the
end.** No source measures that today. `mem.heap.retained` scans the pages of
*freed* ranges; nothing scans the pages of *live* ones, so the resident bytes of
one allocation still held at the end are not measured anywhere, only implied by
a workload that wrote every byte it asked for. The request that would make them
a `KEY` measurement is under "Needs from another stream" in
`docs/open-findings.md`, and it is small: the scan already exists.

**And it does not turn faults into bytes.** The trap the brief names is this
family's own — a derived `faults × page size` column that was wrong by a factor
of four and was deleted. It is not reintroduced here under any name. Bytes come
from `mem.rss` and `mem.heap.retained`, which measure bytes. The section below
on fault counts shows, on this machine, that a fault count can differ by a
factor of a hundred between two allocations of identical size.

---

## The table

One run of each session on the development machine. Every column says what it
rests on, because the table puts a `KEY` column next to a `TEMPORAL` one and a
reader has to be able to tell them apart without reading the code.

### Which allocation — `examples/cross-memory-which-allocation.yml`

| Range | Fate | Faults attributed (`KEY`) | Kernel's own count (reference) | Resident size, same interval (`TEMPORAL`) |
|---|---|---|---|---|
| `0x7fedb21ff010` 8 MiB | kept, still open at the end | **4** | 5 | |
| `0x7fedb19fe010` 8 MiB | kept, still open | 515 | 515 | |
| `0x7fedb11fd010` 8 MiB | kept, still open | 515 | 515 | |
| `0x7fedb09fc010` 8 MiB | kept, still open | 515 | 515 | **+32.08 MiB** as the four opened |
| `0x7fedb01fb010` 8 MiB | freed | 515 | 515 | |
| `0x7fedaf9fa010` 8 MiB | freed | 515 | 515 | |
| `0x7fedaf1f9010` 8 MiB | freed | 515 | 515 | |
| `0x7fedae9f8010` 8 MiB | freed | 515 | 515 | **+32.02 MiB** as they opened, **−32.02 MiB** as they were freed |

The kernel's count is the program's own: `getrusage(RUSAGE_SELF).ru_minflt`
read on either side of each block's `memset`, printed on standard error. It is
the reference because it is independent of this tool and because it follows the
allocation whatever the huge pages did to it. The join is held to it range by
range: never more, and on a run that lost nothing, never more than two fewer —
the two being faults the interpreter can take on its own pages between the two
`getrusage` calls. On the huge-page range the one fault it did not find is
exactly that: a fault inside the write window and outside the range.

The fates are the program's: four blocks kept to the end, four freed. The join
reports them as `still_open` and `closed`, matched to the addresses the program
printed.

### Did it stay — `examples/cross-memory-did-it-stay.yml`

| Step | Resident size (`TEMPORAL`) | Retained (`KEY`, by address) | Reference |
|---|---|---|---|
| four 8 MiB mappings freed | **−32.02 MiB** | +0.00 MiB | `free` unmaps a block glibc served with its own mapping |
| 256 arena blocks of 64 KiB freed, between live ones | **+0.00 MiB** | **+15.00 MiB** | the whole pages those blocks cover: 15 732 736 bytes, printed by the program |

**This is the answer to "did it stay"**, and the separation is the whole
argument: the same call to `free` gave 32 MiB back in one case and kept every
page in the other, and the resident size alone could not tell the second case
from nothing having happened. Across the arena step, the rise in retained bytes
equalled the program's whole-page prediction **to the byte on eight runs out of
eight**, and the resident size moved by exactly zero on all eight.

### The decomposition, and its residue

Every `mem.heap.retained` reading carries `rss` and `live` read at the same
instant as the retained bytes. So a decomposition of the rise in the resident
size can be made from **one event at each end of one run**, and it mixes no
clocks and no runs:

```
decomposition of the rise in rss over the run, one event at each end
  rss       +64.09 MiB
  live      +48.01 MiB
  retained  +14.98 MiB
  residue   +1.10 MiB  of which +1.00 MiB is freed bytes on pages shared with a live block, +0.11 MiB unexplained
```

`live` is the bytes asked for and not freed: 32 MiB of kept mappings and 256
arena blocks of 64 KiB — 48 MiB exactly, plus the interpreter's own 7 KiB. The
program writes every byte it asks for, so here, and only here, asked-for bytes
are resident bytes. (Retained is 14.98 rather than 15.00 because this total
includes the interpreter's 24 KiB fall.)

**The residue is not rounded away, and most of it is predicted.** The retained
scan counts a page shared between a freed block and a live neighbour as live,
which is why the catalog calls the number a lower bound. Those shared pages are
resident, are not live bytes, and are not retained bytes — they are in neither
part. The program says how many there are: 16 MiB freed, 15 732 736 bytes of it
on whole pages, so 1020 KiB on shared ones. Of the 110 KiB left, the four kept
mappings' header pages account for 16 KiB. The other 95 KiB this crossing does
not explain: the interpreter's own growth over two seconds is the likeliest
candidate, and nothing here measures it. The test holds the residue between
zero (nothing accounted for twice) and the predicted part plus one huge page
(the caveat on `mem.rss`: khugepaged can move it by 2 MiB at once, for nothing
the program did).

---

## A fault count is predicted by alignment, not by size, and not by mTHP

The first correlation found ~515 faults for an 8 MiB allocation, where 4 KiB
pages would need 2048. `DECISIONS.md` records the finding and explains it with
**multi-size** transparent huge pages: "the kernel picks larger folios for larger
regions". The engine prints the same explanation under every interval join, and
the `mem.faults` caveat says it too.

**On this machine that explanation is not what happens.** Every multi-size size
is switched off:

```
/sys/kernel/mm/transparent_hugepage/enabled                    [always] madvise never
/sys/kernel/mm/transparent_hugepage/hugepages-2048kB/enabled   always [inherit] madvise never
/sys/kernel/mm/transparent_hugepage/hugepages-{16..1024}kB/... always inherit madvise [never]
```

What happens is simpler, and it predicts every count exactly. With THP
`always`, an anonymous mapping gets one fault per **aligned 2 MiB page it wholly
covers**, and one per 4 KiB page outside them. A block that glibc serves with
its own mapping is `size + 4 KiB` long and starts 16 bytes past a page boundary,
and malloc itself faults the first page when it writes the chunk header. So:

```
faults across the write  =  (aligned 2 MiB pages inside the mapping)
                          + (4 KiB pages outside them)
                          - 1
```

Measured against that prediction, three times at each size:

| Size | Faults, all three runs | Predicted | Huge pages + base pages |
|---|---|---|---|
| 1 MiB | 257, 256, 256 | 256 | 0 + 257 |
| 2 MiB | 512, 512, 512 | 512 | 0 + 513 |
| 4 MiB | 513, 513, 513 | 513 | 1 + 513 |
| 8 MiB | 515, 515, 515 | 515 | 3 + 513 |
| 16 MiB | 519, 519, 519 | 519 | 7 + 513 |
| 32 MiB | 527, 527, 527 | 527 | 15 + 513 |
| 64 MiB | 543, 543, 543 | 543 | 31 + 513 |

Twenty of twenty-one exactly, and the twenty-first is the interpreter's first
fault. `DECISIONS.md`'s own numbers — 263 faults for 1 MiB, 528 for 32 MiB — are
what this predicts, plus a few. The count is nearly flat across sizes because
**every mapping pays about 512 faults for its unaligned edges**, whatever its
size, and a handful for its middle.

The consequence this crossing measured, and the reason the join's "heaviest
ranges" order is the wrong one for the question: an 8 MiB mapping that happens
to land where it covers **four** aligned huge pages costs **4** faults instead of
515. It happened on five runs out of twelve here. Same bytes, same program,
same address space layout rules, and a factor of a hundred and twenty-eight in
faults — and the join ranks that range below every 64 KiB arena block, each of
which costs about fifteen. The tests do not assert which kind a run gets, since
that is where the kernel put a mapping; they assert that the join agrees with
the kernel's count for each range, whichever it was.

Whether the multi-size sizes were enabled when the first correlation was
measured is not recorded anywhere, which is itself the lesson: the THP mode is in
the environment capture for exactly this reason, and the explanation was
written without it. The collector was right both times. The explanation of why
was a hypothesis that nobody measured, and **D4** below is the request to
replace it everywhere it is printed.

---

## Scenarios

### Is every allocation found, with its fate, and with the faults the kernel counted for it?

`every_large_allocation_is_found_with_its_fate_and_the_faults_the_kernel_counted_for_it`,
over `cross-memory-which-allocation.yml`.

The join must say `evidence KEY` and `match_kind interval`. Each of the eight
blocks the program reports must be a range starting at the address the program
printed, of the size it asked for, ending `still_open` if it was kept and
`closed` if it was freed. Its attributed faults must not exceed the kernel's
count for its write, and — when the fault ring lost nothing — must fall short of
it by at most two. On a lossy run the lower bound is not checked, and the test
says so on standard error with the loss numbers rather than passing in silence.

### Does the resident size move by what each phase wrote, in the interval it ran in?

`the_resident_size_moves_by_what_each_phase_wrote_in_the_interval_it_ran_in`.

The `TEMPORAL` half, phrased as its claim. For each of three phases — kept
blocks opened, returned blocks opened, returned blocks freed — the reading
before the phase's first range event and a reading inside the 300 ms of quiet
after it differ by the bytes the phase wrote, give or take one huge page. The
timestamps come from the join's `opened_ns` and `closed_ns` and from the raw
`mem.rss` events, which share `CLOCK_MONOTONIC`. Nothing in the check says which
range the bytes belong to.

### Does one free give memory back while another keeps it?

`one_free_gives_memory_back_and_the_other_keeps_it`, over
`cross-memory-did-it-stay.yml`.

Two frees, two fates, told apart. The unmapped blocks must take 32 MiB off the
resident size and add nothing to retained; the arena blocks must leave the
resident size where it was and add exactly the whole pages the program reports.
The session has no allocation events to align on — it cannot, D3 — so the
phases are found by the one number the program fixes exactly: the live bytes,
which move in steps of 32 and 16 MiB.

### Does the rise in the resident size decompose, and is the residue shown?

`the_resident_size_is_live_plus_retained_plus_a_residue_that_is_printed`.

The decomposition above, from the first reading of the running program and the
last one. Live must be what the program holds to within the interpreter's
churn; the residue is printed with its predicted and unexplained parts, and
bounded as described.

### Do both halves measure the same program?

`both_halves_of_the_crossing_launch_the_same_program`. The two sessions have to
be two (D3), so what makes their tables comparable is that the program is the
same. The test reads both files and requires every request of both to name one
identical launch. A workload edited in one and not the other would make the
second half of the crossing about a different program.

---

## Workloads

One program, in both sessions, written once in each file as a YAML anchor so
every request of a session provably follows the same launch.

| Phase | What it does | What it guarantees |
|---|---|---|
| setup | `mallopt(M_MMAP_THRESHOLD, 1 MiB)` | every 8 MiB block is its own mapping, unmapped by `free`; fixing the threshold also stops glibc raising it after the first mapped free, which would move later blocks into the arena |
| quiet | 300 ms | a baseline before anything is allocated |
| kept | four 8 MiB blocks, every byte written | 32 MiB live and resident to the end |
| returned | four more, written, and after 300 ms freed | 32 MiB resident, then given back |
| held | 512 blocks of 64 KiB from the arena, written, and after 300 ms every other one freed | 16 MiB freed into holes between live blocks: retained, not returned |
| self-report | `BLOCK <fate> <address> <size> <faults>` per large block; `ARENA <blocks> <bytes> <whole-page bytes>` for the freed arena blocks | the kernel's fault count per allocation, and the most a retained scan can find |

Each session runs in about two seconds and the whole of `tests/cross_memory.rs`
in about two, run in parallel.

---

## What cannot be verified here

**Per-allocation residency at the end.** Covered above: no source scans the
pages of a live range. The table's "kept" rows are resident because the program
wrote them and the resident size rose by their size in their interval — which is
the workload's guarantee plus `TEMPORAL` evidence, not a measurement of the
range.

**The two sessions cannot be one, so no number is added across them.** The fault
table and the retained table are from two launches of the same program, and
nothing is summed, subtracted or compared between them. D3 is why.

**The first reading of a launched program is not of the program.** A poll
taken as the run starts reads the child before its first allocation, and the
race goes one of two ways. On some runs `mem.heap.retained` publishes `bytes=0
live=0 rss=8192` — a process with no heap yet; on others the read fails with
`Permission denied` on `/proc/<pid>/pagemap` and is counted as a failed read,
because a child that has not yet exec'd is still this tool's non-dumpable image.
Both outcomes are surfaced, and `mem.rss` shows the same first point, which
`docs/scenarios/mem.md` D1 already prints. The tests here take their baseline
from the first reading of the running program, and assert nothing about which
way the race went.

**THP state is the machine's.** Whether a given 8 MiB mapping costs 4 faults or
515 is where the kernel put it, and huge pages may be unavailable on a
fragmented machine. The checks hold the tool to the kernel's own count, so
they are true whichever happens; the alignment table above is a measurement
of this machine and is not asserted by any test.

**`make check` on a loaded machine.** Five tests this stream does not own are
known to fail under load (`docs/scenarios/mem.md` has the list and the
numbers). Nothing in `tests/cross_memory.rs` measures cpu time or scheduling,
and its tolerances are resolutions rather than calibrations, so it is not among
them — but a machine running six streams at once is exactly where that would
show. So it did, and here are the numbers rather than a claim of a clean run.

`tests/cross_memory.rs` passed on every run: five in a row on its own, and in
each of four full `make check` runs taken while the other streams of this wave
were running (load average 9 to 23 on 24 cpus). Those four `make check` runs
were not clean. The first failed only on this stream's own clippy lint, since
fixed. The other three each failed in a *different* test of another family,
and each of those passed when run on its own a minute later:

| Run | Failed | What it saw |
|---|---|---|
| 2 | `scenario_mem::an_exact_number_of_allocations_is_reported_exactly` | the shim dropped 13 of 20 013 records, counted, after a read round ran 84 ms late |
| 3 | `scenario_io::the_tracer_and_the_kernels_own_counter_see_the_same_requests` | 14 462 976 bytes traced for 16 777 216 read |
| 4 | `scenario_cpu::a_known_number_of_retired_instructions_is_counted_as_that_number`, and the format test beside it | 1 016 919 080 instructions for 1 000 000 000, +1.7 % |

Each is a test asserting what the machine was doing, which is the shape
`STATE.md` warns about, and none of them touches a file this stream wrote. They
are the coordinator's to weigh at the merge, on a quiet machine.

---

## Defects found

Four. None is in a file this stream owns, so each is pinned by a test or
recorded, and requested under "Needs from another stream" in
`docs/open-findings.md` with the change wanted.

### D1 — a join on `task` credits each reading to one allocation by recency, and says only `KEY`

Pinned by `d1_a_join_on_task_credits_each_reading_to_the_latest_allocation_and_says_temporal_within_it`,
which asserts the fix since the eighth wave: the correlation carries
`within_key: TEMPORAL` beside `evidence: KEY` (`docs/families/join.md`).

The only rule that validates between an allocation and a resident size, other
than D2's, is `eq` on `task`, since both carry it:

```yaml
joins:
  - {id: task-eq, from: mem.heap.alloc, to: mem.rss, via: task, match: eq}
```

```
join task-eq  mem.heap.alloc -> mem.rss  evidence KEY
  20 of 21 `mem.rss` events attributed (95.2 %); 1 carried a key no open state had
  states: 5075 opened, 5074 closed, 1 still open at the end, peak 1 held at once

  heaviest keys
    KEY                                            EVENTS  OUTCOME
    task:53829                                          4  closed
    task:53829                                          4  closed
    ...
```

Two claims are in that output, and they deserve different words. **That each
reading and some allocation belong to one process** is a shared key, and `KEY`
is right for it. `tests/join_engine.rs` holds `proc.threads -> proc.fd` to
exactly that, and it should stay `KEY`.

**Which allocation each reading is credited to** is not a key at all. A task
names the process, not the allocation, so every allocator call replaces the
state before it (`peak 1 held at once`, over five thousand replaced) and each
reading goes to whichever call came last. Most often that is a two-byte string
of the interpreter's. The rows under "heaviest keys" are that crediting: one
row per allocation, each holding the readings that happened to follow it. That
is `TEMPORAL`, and the correlation says `KEY` and nothing else. A reader who
wants "which allocation cost memory" and writes the first rule that validates
is handed an answer by recency that is labelled as an answer by key.

The request is not to demote the join. It is to say both things: `KEY` for the
association, and `TEMPORAL` for the crediting within a key whenever a state was
replaced before its events arrived.

### D2 — a join on a key no event carries was refused after the run — **fixed**

Pinned by `d2_a_join_on_a_key_no_event_carries_is_refused_before_the_run_is_paid_for`,
which now asserts the fix rather than the defect.

`catalog/mem.yaml` declares `keys: [task, cgroup, mem]` on `mem.rss`, and `[task,
mem]` on `mem.heap.live` and `mem.heap.retained`. None of the three collectors
emits anything but `task:tid`. So the rule a reader would write first —
`interval` on `mem` from an allocation to a resident size — passes `omn
validate`, runs the whole measurement, and then:

```
error: rule `pages-of-allocation` joins on `mem`, which `mem.rss` events do not carry
exit 1
```

with nothing on standard output. The refusal is right and it is late:
`join::check` exists so that a rule that cannot match is found before a
measurement is paid for, and it cannot find this one, because the validator
trusts the catalog's `keys`. It was the sixth wave's finding about
`proc.syscall` and `mem.kmalloc` in three more places, and the first of them to
cost a run rather than an empty table.

**What closed it**, in the coordinator's pass at the end of this wave: `mem` is
gone from the three entries, so the validator refuses the rule with exit 2
before anything is attached, naming the keys the source does carry. Six shipped
sessions asked for the key and were told it was carried —
`mem-fragmentation.yml`, `mem-backend-agreement.yml`, `mem-capability-sweep.yml`,
`mem-footprints.yml`, `mem-retained-eviction.yml`, `mem-rss-run.yml` and
`mem-working-set.yml` — and each now asks for what its source has. The join a
reader wants instead is the one D1 describes, and the `between` match kind is
what would give it the evidence level it deserves.

### D3 — allocations and retained memory cannot be asked of one program, and the refusal blames the request

Pinned by `d3_two_shim_sources_on_one_launch_are_refused_as_a_request_rather_than_as_the_build`.

```
error: two collectors disagree about `OMN_HEAP_FD` in the launched program's environment (`4` and `6`)
exit 2
```

Each shim-backed source opens its own pipe and gives the child its descriptor,
and one child can hold one. The refusal is honest — neither pipe wins silently —
and it comes before the launch. Two things are wrong with it:

- **It exits 2**, "the request cannot mean anything", because
  `CollectError::ConflictingEnvironment` is classed `Whose::Request`. This
  request means something, `omn validate` accepts it, and it is the build that
  cannot serve it: `DECISIONS.md` settled that as exit 3.
- **It is why this crossing is two sessions.** "Which allocation cost memory and
  did it stay" is one question about one program, and the tool cannot ask it in
  one run. One shim stream per launch, fanned out to every preload collector
  that wants it, would let the whole crossing be one session and the
  decomposition be joined to the ranges.

### D4 — the explanation printed under every interval join is contradicted by the measurement

Not pinned: the engine's note is text in `src/encode.rs`, which stream E owns
this wave, and a test asserting that it is wrong would only assert its wording.

Every `mem.heap.alloc -> mem.faults` join prints:

```
EVENTS counts faults, not bytes, and on a kernel with multi-size transparent huge pages it does
not even rank by bytes: the kernel picks larger folios for larger regions, so a 1 MiB and a
32 MiB allocation both fault a few hundred times.
```

The first half is right and is the important half. The second is a mechanism,
and on this machine it is not the mechanism: every multi-size size is `never`,
and the counts are predicted exactly by 2 MiB alignment, as measured above.
The same sentence is in the `mem.faults` caveat in `catalog/mem.yaml`, in
`DECISIONS.md` and in `docs/scenarios/mem.md`. What the note should say is what
is true on every kernel: fault counts depend on how a range is backed —
huge pages, multi-size folios, or base pages — and on where it lands, so two
ranges of one size can differ by two orders of magnitude, and the THP mode is
in the environment capture.

---

## What a later wave should take from this

- **The engine has no way to say `TEMPORAL`.** No match kind produces it — D1 is
  the engine producing it by accident and calling it `KEY` — so the
  one honest way to put a resident size beside a range today is to leave it out
  of the join — which is what these sessions do — and do the arithmetic in a
  test. A match that credits each reading's change to the states that opened or
  closed since the reading before, and always says `TEMPORAL`, would make the
  second column of the first table a join line rather than prose. It is
  requested with its signature.
- **Per-range residency is the cheapest `KEY` this crossing is missing.** The
  pagemap scan that `mem.heap.retained` runs over freed ranges would, run over
  live ones and published per range under `mem:[base,len)`, make "the resident
  pages it still holds at the end" a measurement of the allocation rather than
  a guarantee of the workload.
- **Ranking attributions by count is ranking by the thing this family warns
  against.** For an interval join onto faults, "heaviest ranges" orders by
  faults; the question people bring is bytes.
