# Family `io`

Fourteen sources across three layers that must never be reported as one
another: what the application asked for (the syscall), what the filesystem did
with it (journal, locks, allocation), and what the device actually performed
(the block layer). A read that takes 40 ms at the syscall and 2 ms at the
device was not slow because of the disk, and the gap between the layers is the
diagnosis rather than an error to be smoothed over.

This document records what is implemented, what was measured while doing it,
and what the family cannot do until the shared layer grows.

## What is collected

Nine sources over two mechanisms that answer different questions and are never
a fallback for each other.

Five through procfs, all system-wide. They are free, they are exact, and they
say nothing about who caused the I/O.

| Source | Backend | What an event carries |
|---|---|---|
| `io.bytes` | `fs:proc` (`/proc/diskstats`) | bytes the devices moved during the tick, split into reads and writes |
| `io.iops` | `fs:proc` | operations the devices completed, with discards and flushes counted separately |
| `io.merges` | `fs:proc` | requests the block layer combined |
| `io.inflight` | `fs:proc` | requests outstanding at the instant of the read |
| `io.pressure` | `fs:proc` (`/proc/pressure/io`) | the share of the tick during which something was stalled on I/O |

Four through the block layer's own raw tracepoints, one request at a time.

| Source | Tracepoints | What an event carries |
|---|---|---|
| `io.latency` | `block_rq_issue` -> `block_rq_complete` | one request: how long the device held it, its device, sector, size, operation letters, submitting task and status |
| `io.queue` | `block_rq_insert` -> `block_rq_issue` | the same request, and how long it sat on a software queue before the driver was handed it |
| `io.size` | `block_rq_issue` | the length of one request at the moment it was issued |
| `io.writeback` | `writeback_start` -> `writeback_written` | one turn of the flusher's loop: how long it ran, how many pages it wrote, which filesystem |

Five remain (`syscall`, `fs`, `pagecache`, `readahead`, `fsync`) and are named
below with what each is blocked on. `io.bytes` and `io.pressure` also declare a
`fs:cgroup` backend -- `io.stat` and `io.pressure` under a cgroup -- which waits
for a cgroup target.

## What the implementation had to decide

**Counters are reported as increases, never as their value.** `/proc/diskstats`
has been counting since boot, so a single read is a number about the machine's
lifetime. An event therefore carries the increase over the tick that produced
it, together with `interval_ns`, the length of that tick. Summing the increases
gives the run's total and dividing by the window gives its rate; had the events
carried the running total, both would have been nonsense. A `count` request
skips the intermediate reads entirely and reports one total for the whole run,
measured from a baseline taken before the observed program starts.

**The first read of a polled run is a baseline and produces no event, and the
run's end produces none either.** The runner samples every collector and *then*
decides whether to stop, so a read at either edge covers a window of
microseconds. Taken at the head, it put a `0.000` at the start of every pressure
series; taken at the tail, it added a second point at the same offset as the
last tick. Both were arithmetic about a window near zero, and both dragged the
summary that followed them. A counted run is the exception: its baseline is
taken in `start`, before the observed program is released, so its single total
covers everything.

**Adding up every line double counts, on this machine by a factor of two.** A
partition's I/O is counted again on the disk that holds it, and a
device-mapper or md device's on each member below it. Measured here, over the
same file at the same instant:

```
naive sum over every line: 61.5 GiB
leaf devices only:         30.7 GiB
```

`nvme0n1` and its eight partitions are one device counted twice. So the sum runs
over the devices nothing is stacked on, and *what a device is comes from `/sys`,
not from its name*: `nvme0n1p6` looks like a partition and is one, `dm-0` looks
like a disk and is stacked on whatever is below it, `zram0` looks synthetic and
is genuinely its own device. A device `/sys/class/block` says nothing about is
left out of the total and counted as an undecoded record, rather than being
assumed either way.

**A sector here is 512 bytes**, whatever the device's logical block size -- the
kernel normalizes before writing the file. Bytes are `sectors x 512` exactly,
which is a fact about the file rather than a conversion that has to be
justified. This is the one place in the family where a derived number is safe,
and it is worth saying why: `faults x page size` was wrong by a factor of four
in the `mem` family for exactly the reason that does not apply here.

**Columns that a kernel does not write are absent, not zero.** Discards arrived
in 4.18 and flushes in 5.5. Reporting zero for them on an older kernel would
claim the device discarded nothing, which is a measurement nobody took.

**A counter that goes backwards voids its interval.** A device removed and
re-added restarts at zero; subtracting would report an enormous number. The
interval is dropped, counted as a loss, and the reason says what happened. The
same holds when the set of devices changes between two reads: the difference
would be part I/O and part hotplug.

**Pressure is two different measurements and they do not share a name.** A poll
computes the stalled share of its own window from the `total` counter and
reports it as `stall_ratio`. A snapshot observes no window at all, so it reports
the kernel's decayed ten-second average under the name `some_avg10`, with a
`basis` attribute saying which it is. Every ratio is on the `[0, 1]` scale; the
kernel writes percentages and that division is the only conversion.

## What was measured

Sixty-four mebibytes written to a file and flushed, on btrfs over an NVMe
partition, seen from the device's side:

```
io.bytes  bytes=67375104 write_bytes=67375104 read_bytes=0 sector_bytes=512
          interval_ns=378917944 devices=nvme0n1,zram0
io.iops   ops=530 writes=530 reads=0 discards=0 flushes=1
```

64 MiB asked for, 64.25 MiB written: 266 KiB of filesystem metadata that the
application never mentioned. And 530 operations for 64 MiB is 127 KiB each --
the block layer merged the writes on the way down. Both numbers are the point of
the family: this is what the *device* did, and it is not what the program asked
for.

And the same family driven from the command line, while `dd` wrote 200 MiB and
flushed it:

```console
$ omn io.bytes --system -m poll@300ms -e timeline,summary -d 2s --backend force:fs:proc
io.bytes  via fs:proc  (io.bytes)
  8 events over 2.10s, stopped because the requested duration elapsed
  no loss: every read succeeded and no tick ran late
  degraded: fs:proc: system-wide, per device

  timeline
    system
      +0ns        0 B
      +300ms      200.5 MiB
      +600ms      0 B
      ...
```

200 MiB asked of `dd`, 200.5 MiB seen at the device, in the tick that contained
the flush.

## Ground truth

`bytes_flushed_to_a_disk_show_up_in_its_counters` writes 32 MiB of
incompressible data to a real block device, flushes it with `fsync`, and
requires that the collector report at least that many bytes written, and no more
than the kernel's own counter moved over a window containing the run's. It skips
itself, rather than failing, where there is no block device underneath -- a
tmpfs or a container overlay -- which it establishes from `/proc/self/mountinfo`
and not from `stat`: btrfs gives its subvolumes an anonymous `st_dev` with major
0 that appears nowhere in `/sys/dev/block`, and an earlier version of the test
silently skipped on this machine for that reason.

`the_reported_stall_is_bounded_by_the_kernels_own_counter` holds the pressure
collector between two direct reads of `/proc/pressure/io` and requires that the
stall it reports fit inside the stall the kernel counted over the wider window,
and that no window be stalled for longer than itself.

The arithmetic is checked against a synthetic `/proc/diskstats` written by the
test, so a known increase must come out as a known number of bytes.

What is still missing is an *injected* delay: `dm-delay` under a known workload
would let the latency and pressure numbers be checked against a delay that was
put there on purpose. It needs root, so it belongs with the phase 8 harness
rather than with `cargo test`.

## Needs from the shared layer

The five collectors run end to end. What follows is what the family still
cannot express, in files this thread does not own.

### 1. A `system` target has to reach the collector -- landed

`Prepared::attach` now takes the target rather than a pid, so a source with no
process behind it binds to `Target::System` and refuses anything else by name.
This family's collectors do exactly that: `/proc/diskstats` counts per device
for the whole machine, so a `--pid` is refused rather than answered with the
machine's total.

The rest of this section is what is still missing.

### 1b. The closing read of a polled counter is noise, and `net` emits it

`net/dev.rs` closes its window in `stop` -- "without a final read the last
interval of every run is silently dropped" -- and with this runner that read
always covers the few microseconds between the last tick and the break, because
the loop samples and only then checks whether to stop. The `io` collectors did
the same for one commit and it showed up as a second point at the same offset as
the last tick, with a value of zero. Either the runner should stop before its
last sample rather than after it, or a collector should not read at the edge;
whichever is chosen, the two families should agree, and today they do not.

### 2. A device key, so a series can be per device

`emit::grouped` keys a series on `event.keys.first()`, and `KeyValue` has no
variant for a block device. So these collectors emit one event per tick holding
the sum over every leaf device, with a `devices` attribute naming what was
summed. The sum is correct -- leaf devices do not overlap -- but the per-device
breakdown that the catalog promises through `topk` cannot be expressed at all,
and a machine with an NVMe and a spinning disk reports one number for both.

Wanted, in `src/event.rs` and `src/request.rs`:

```rust
/// A block device, by the major and minor the kernel names it with.
KeyValue::Dev { major: u32, minor: u32 }   // renders as `dev:259+0`
KeyKind::Dev                                // written `dev` in a catalog entry
```

With it, each device gets its own series, `topk` becomes the busiest devices,
and the `dev` filter field that four `io` entries already declare has something
to match against. `net` will want the same thing for an interface, and the
`Iface` key being added there is the same shape.

### 3. A backend has to be able to declare which targets it serves

`catalog::Backend` has no target information, so the resolver's only rule is the
entry-level `targets` map plus "a `bpf_only` target needs a `bpf` backend".
For `io.bytes` the chain is `bpf:tp` then `fs:cgroup` then `fs:proc`, and a
`--system` request therefore resolves to `fs:cgroup`, a backend whose whole
purpose is per-cgroup attribution:

```console
$ omn io.bytes --system -m poll@200ms -e timeline -d 1s
error: no collector is written for `io.bytes` on backend `fs:cgroup` yet
```

Reordering the chain inside this family's own file would fix that request and
break the opposite one: `--cgroup` would then land on `/proc/diskstats` and be
answered with the whole machine's I/O, which is the worst bug this tool can
have. The fix is not in the catalog data, it is that a backend should say what
it can be scoped to:

```yaml
- {id: "fs:cgroup", requires: ["cgroup2:io.stat"], targets: [cgroup, container], ...}
- {id: "fs:proc",   requires: ["/proc/diskstats"], targets: [system], ...}
```

and the resolver should skip a backend that cannot serve the requested target,
the way it already skips one whose requirements are missing, and say so in the
chain it prints.

### 4. A filter is validated, placed, and then never applied

`--filter` is checked against the source's fields and the resolver decides where
it would run -- `FilterPlacement::Userspace` for every procfs backend -- but
nothing in `collect`, `emit` or `main` ever evaluates it. The events come back
unfiltered:

```console
$ omn mem.rss --pid $$ -m poll@100ms -d 300ms -e summary --filter "comm == 'nothing-like-this'"
mem.rss  via fs:proc:smaps_rollup  (mem.rss)
  4 events over 300.31ms, stopped because the requested duration elapsed
```

Four events matched a predicate nothing can satisfy. This is not specific to
`io`, but it is what the family's `dev` and `rwbs` filter fields are for, and it
is the kind of silent difference between what was asked and what was answered
that the project exists to refuse. `join.rs` already evaluates the same
predicates in userspace, so the machinery exists; what is missing is applying
`request.filter` to a `Collection` before aggregation, and reporting how many
events it removed.

## Traps recorded for the eBPF work

From the catalog entries, so that the wave 2 thread
does not rediscover them:

- `io.latency --pid` is reliable **for reads only**. Reads are submitted in the
  reader's context, so task storage is readable at `block_rq_issue`
  (`IDENTITY`); writes leave through writeback in a kworker and fall back to
  `file:dev+ino` (`KEY`). The evidence level differs per direction and has to be
  reported per direction.
- Request pointers are recycled. The key is `(pointer, open timestamp)`, scoped
  equality, never a bare pointer.
- A cold page cache and a warm one differ by an order of magnitude. The protocol
  has to declare which it wants and enforce it, or the distribution comes out
  bimodal and gets called noise.
- SSDs drift: internal garbage collection, write cache, SLC buffer. Precondition
  and watch inter-run drift, or test ordering decides the result.

---

# The block layer, through its own tracepoints

The second half of this document is the sixth wave's: four sources that see one
request at a time rather than a device's counters. What follows is what had to
be decided to write them, what was measured while doing it, and what the family
still cannot do.

## The two questions the shape answers

A block request passes three moments the kernel publishes by name, and every
number this half of the family wants is the *gap* between two of them:

```
  insert ----------- issue ----------- complete
         `io.queue'         `io.latency'
         the software queue what the device did
```

So one shape serves all three block sources. An **open** tracepoint writes the
moment and the submitting task into a map keyed by the request pointer, and a
**close** tracepoint reads it back, subtracts, publishes one record and removes
the entry. `io.size` is the degenerate case with no open at all: a request's
length is readable at the issue itself.

**These stream rather than aggregating in the kernel, and the reason is not the
volume.** `src/bpf/runqlat.rs` builds its histogram in the kernel because a
wakeup happens hundreds of thousands of times a second and one record per wakeup
would make the measurement cost grow with the workload. Block completions are
rarer than that by three orders of magnitude on an ordinary machine. But the
reason these stream is the **key**: `io.latency` and `io.queue` are the only
sources in the project that produce `blk:rq@open_ts`, and a kernel-side
histogram keyed by `(device, bucket)` throws that identity away. A join needs
the requests one at a time or it has nothing to join.

What it costs is written down where a reader will meet it: the loss path below,
and `examples/io-ring-overflow.yml`.

## What the implementation had to decide

**The scope of the key comes from the kernel, and it had to.** A `struct
request` is freed the instant it completes and handed straight back out, so the
catalog writes `blk:rq@open_ts` rather than `blk:rq`. The obvious scope -- the
moment the program first saw the request -- is wrong in a way that is hard to
see. `io.queue` and `io.latency` are two requests, therefore two independently
loaded program sets, each calling `bpf_ktime_get_ns` at its own instant; the
same request would carry two different scopes, the join between them would match
nothing, and it would report that as a finding about the machine rather than as
its own bug. `request.start_time_ns` is the kernel's own stamp, taken once when
the request was created, in `CLOCK_MONOTONIC` like everything else here -- so two
programs that never met agree on it exactly. Measured: 1273 completions joined
to their issues, 0 unmatched.

It is zero on a queue the kernel keeps no statistics for. Those events carry
`scope = run` and the collector counts them, because a scope that will not join
across collectors must not be indistinguishable from one that will.

**`[none]` means a queueing time of zero, not a measurement that failed.** On a
blk-mq device with no I/O scheduler -- the default for NVMe, and what this
machine runs -- most requests are handed to the driver directly and never go on
a software queue. `block_rq_insert` does not fire for them at all. The first
version of `io.queue` counted those as closes with no open and reported zero
events over a device doing two hundred megabytes a second, which says *this tool
cannot see your queue* when the truth is *your device does not queue*. Those are
opposite answers.

They are told apart by the kernel's own creation stamp: a request created
**after** the programs attached would have been seen at its insert had there
been one, so the absence is the measurement and the event goes into the
distribution at zero with `queued = direct`. A request created before them is
older than the measurement and is in no event. Measured over a 300 MiB write:
1657 requests, 1563 direct, 94 that actually queued, with a mean of 2.94 us and
a maximum of 87.77 us.

**The device is the series and the pair is the key.** A latency averaged over an
NVMe and a spinning disk describes neither, and no later layer can split one
back apart -- which is this family's own recorded lesson. Every event is keyed
first on `KeyValue::Dev`, then on the request, then on the submitting task. The
name is an attribute and the major:minor is the key, because a device name is a
symlink away from meaning something else. The block layer reports the
*partition*, so these say `nvme0n1p6` where `/proc/diskstats` sums into
`nvme0n1`; the two are not interchangeable and are not made so.

**`rwbs` is the kernel's own letters, deliberately.** `blk_fill_rwbs` is what
`blktrace` and every block tracepoint in tracefs print, so somebody filtering on
`rwbs == 'WS'` is filtering on something they have already seen. The raw
`cmd_flags` travels beside it, so a letter this decoder got wrong can be seen to
be wrong rather than only being wrong.

**`enum wb_reason` travels as a number and is not named.** This kernel has no
`WB_REASON_LAPTOP_TIMER` and numbers everything after it one lower than a 5.x
kernel does. A name from a table compiled into `omn` would be a wrong answer
that reads exactly like a right one. `sync_mode` *is* named, because
`WB_SYNC_NONE` and `WB_SYNC_ALL` have been those two values since writeback
existed.

**`nr_pages` is a budget being spent, not a total.** `wb_writeback` subtracts
each round's progress from `work->nr_pages` before firing `writeback_written`,
so the pages a round wrote are the difference between two readings. Reading
either alone gives the remainder, which for a `sync` is `LONG_MAX`.

**A close with no open is counted and is not a loss.** A loss is something the
tool had and could not keep. A completion for a request that was already in
flight when the programs attached is something that never existed for this run,
and reporting it as a drop would put it in the same number as a record the ring
could not hold. It is a note with its own count.

## What was measured

**A known number of requests, found one for one.** 4096 one-page `O_DIRECT`
reads issued on one thread, watched with `io.size --tid`:

```
4096 events, every one of them exactly 4096 bytes, nothing else in the series
```

Not "about four thousand". The page cache is bypassed so every read reaches the
block layer; the reads are issued one at a time so the block layer has nothing
to merge them with; each is one page at a page-aligned offset so none spans two
extents. One read, one request, one page. This is the sharpest ground truth the
family has and it is one `/proc/diskstats` cannot give at all.

**The caveat about writes is half right, and the half it gets wrong is the half
a reader meets first.** The catalog says, of `io.latency`:

> Reads are attributable to the submitter (IDENTITY); writes are not, they leave
> through writeback in a kworker and fall back to file:dev+ino (KEY).

Measured against a launched program that writes 64 MiB, `fsync`s, drops the page
cache and reads the same 64 MiB back:

```
 RA: 512 requests, 64.0 MiB    the read back
 WS: 511 requests, 63.9 MiB    the data written
WSM:  22 requests,  0.3 MiB    filesystem metadata
WFSM:  1 request               the flush that ended the fsync
```

Both halves are there, and both are attributed to `python3`. An `fsync`ed write
is submitted **in the calling task's own context** -- `fsync` does not return
until the data is on the device, and the extents go down from inside the
syscall. What is genuinely unattributable is *deferred* writeback: a page
dirtied and left alone reaches the device thirty seconds later from a kworker,
and by then there is no submitting task anywhere in the block layer.
`examples/io-my-own-reads.yml` is that correction as a measurement.

The difference is the difference between the I/O a program *waited for* and the
I/O it *caused*, and those are the same number only when it fsyncs.

**The scope separates a recycled pointer, and this is how far.**
`blk_time_get_ns()` caches its reading for the lifetime of the submitting task's
plug, so every request created inside one plug section carries the same
`start_time_ns`. Two lives of one pointer inside one section are one key.
Measured under eight concurrent one-page readers:

```
166448 events, 527 pointers, 165921 reuses, 5722 keys used twice
```

96.6 % of the reuses separated, where a bare pointer would have separated none.
The residual is a limit of the key rather than of this collector, and item 5 of
the sixth wave's section in `docs/open-findings.md` says what would close it.

**The loss path, driven on purpose.** The ring holds 256 KiB, a record is 96
bytes plus an 8-byte header, and it is drained every ten milliseconds, so it
overflows above roughly 250 000 requests a second. Thirty-two concurrent
one-page readers get there:

```
609880 events over 3.02s
LOSS: 93274 dropped, 0 failed reads, 8 late ticks, 0 undecoded, 0 throttled
  8x a round of reads outran the 10.0ms tick: worst 10.6ms
  194x the block ring was full: requests completed faster than they were drained
```

Ninety-three thousand admitted missing out of seven hundred thousand. That
second number is the point: the BPF ring buffer does not report its own
overflows, so without the program's own drop counter a distribution built from
the first number would be fifteen percent short and nothing anywhere would say
so -- and the missing fifteen percent are the ones that arrived during the
bursts, which is when the interesting latencies happen.

**What a run of these costs.** `io.size --system` over a device doing 200 000
requests a second charged 7.1 % of the observed workload's own cpu. That is
above what anyone would set `--budget` to, and it is the honest price of one
record per request at that rate; at the few thousand a second an ordinary
machine produces it is 0.06 %.

## What this half cannot do

**`io.writeback` sees the flusher and not a file.** The catalog lists `file`
among its keys and these two tracepoints cannot produce it: a round walks a
superblock's dirty inodes and `writeback_start` and `writeback_written` see the
work item, not the inodes. The finest attribution available is the filesystem,
and every run says so rather than emitting events with an empty key for a join
to attribute nothing to.

**And on btrfs it sees a small fraction of what was written.** 256 MiB written
buffered and then `sync`ed came back as six rounds and 976 pages -- 3.8 MiB.
This is not a defect in the collector: btrfs writes its delayed-allocation
extents through its own workers on the transaction commit, not through
`wb_writeback`, so the generic flusher's budget never accounts for them. On a
filesystem that does use the generic path the number would be the write. There
is no ext4 or xfs mounted on this machine, so the page count is **not** checked
against a known quantity anywhere in the suite, and
`a_sync_produces_writeback_rounds_that_waited_for_the_device` checks the
property that can be created here instead: a `sync(2)` inside the window must
produce at least one `WB_SYNC_ALL` round, because `sync` does not return until
it has.

**A cgroup target is refused rather than served.** `io.latency`, `io.queue`,
`io.size` and `io.writeback` all declare one and this build filters by thread
group and thread id only. The refusal is exit 3 -- a gap in the build, not in
the machine -- and names `docs/open-findings.md`, where the request for
`bpf_get_current_cgroup_id` is written down.

**A filter runs here, not in the kernel.** The resolver credits any backend
whose id begins with `bpf` with pushing the predicate down, and that credit comes
from the backend's *name*. So the collector applies the predicate itself, over
the drained records, and says in the run's own output that it did: the numbers
are then what the resolver claims, and the cost is not. This is the same
compromise `sched.runqlat` made and the same one it recorded.

**`--keys` is validated and never applied.** `omn io.latency --keys blk,task` is
checked against the entry, reported in the resolved file, and then ignored --
the series is whatever the collector put first, which here is always the device.
Nothing in `collect`, `emit` or `main` reads `request.keys`. It is the same
shape as the filter gap this document recorded two waves ago and it is not
specific to `io`; it is worth a finding of its own when somebody owns the emit
layer.

## The five that are left, and what each waits for

| Source | Waits for |
|---|---|
| `io.pagecache` | a `Btf::member_offset` that descends anonymous members, to reach `folio.mapping`. Item 2 of the sixth wave's needs. |
| `io.readahead` | the same, plus the catalog entry that replaces its glob with the two tracepoints named in item 4. |
| `io.fs` | **fentry.** Its first backend is `bpf:fentry` on `vfs_read`, and the `tracepoint:ext4:*` fallback is deliberately not converted: choosing one filesystem's tracepoints would answer for ext4 on a machine running btrfs. |
| `io.syscall` | fentry on `vfs_read`/`vfs_write`. Its `bpf:raw_tp` fallback on `raw_syscalls:sys_enter` exists and is what stream D is doing for `proc.syscall`; the two should share a program rather than each writing one. |
| `io.fsync` | fentry on `vfs_fsync_range`. |

Three of the five are the same missing mechanism. `docs/bpf-layer.md` names it
under "What is not here yet", and it is now the single largest thing standing
between this family and the layer above the block one.

## Notes for whoever comes next

- **Check the prototype before building instructions.** `Btf::tracepoint_
  signature` is the guard and `src/bpf/io.rs` uses it on all four tracepoints.
  A raw tracepoint hands over an array of `u64` and nothing in it says what any
  slot holds, so a program written for one prototype reads a `struct request *`
  out of a slot holding an integer and gets a plausible latency -- which is
  worse than an error, because a plausible latency is one nobody questions.
- **Clear the whole record before filling it.** Twelve stores rather than
  tracking which holes were left. An uninitialised byte in a published record is
  whatever the last program to use that stack left there, reported as a
  measurement.
- **`__data_len` is decremented by `blk_update_request` *after*
  `block_rq_complete` fires**, so it is still the request's length at both the
  issue and the first completion. A partially completed request is measured to
  its first completion and the later ones are counted as unpaired; the
  `done_bytes` on the event is the fourth tracepoint argument, which is what
  *that* completion accounted for.
- **`request.part` is null for a passthrough request.** Those are reported under
  device 0:0 and counted, rather than being dropped or attributed to whatever
  device happened to be first.
- **The scratch file has to be on a block device.** `/tmp` is a tmpfs on this
  machine, and half a day went into a collector that looked broken because every
  workload written to `/tmp` produced no block request at all.
  `CARGO_TARGET_TMPDIR` is inside the build directory, which is on the
  repository's own filesystem, and `tests/scenario_io.rs` says so where it
  matters.
