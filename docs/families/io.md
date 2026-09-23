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

Three more -- `io.syscall`, `io.fs` and `io.fsync`, the layers above the block
one -- collect through fentry since the eighth wave, and are the third part of
this document. Two remain (`pagecache`, `readahead`). `io.bytes` and
`io.pressure` also declare a `fs:cgroup` backend -- `io.stat` and `io.pressure`
under a cgroup -- which waits for a cgroup target.

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

## The five that were left

Three of them -- `io.syscall`, `io.fs` and `io.fsync` -- are the third part of
this document: the eighth wave wrote fentry, and they collect through it. The
two that remain are `io.pagecache` and `io.readahead`, which wait on a
collector rather than a mechanism since `Btf::member_offset` learned to descend
anonymous members in the seventh wave; neither was this stream's to write.

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

---

# The layers above the block one, through fentry

The third part of this document is the eighth wave's: three sources over a new
attach kind, `BPF_PROG_TYPE_TRACING` with an `attach_btf_id`, and the first in
this family that see a *call* rather than a request. What follows is where each
is attached and why, what that took, what was measured, and three ways a call
was lost that nothing in this project counted until now.

| Source | Timed at | Shape | An event carries |
|---|---|---|---|
| `io.syscall` | `__x64_sys_read`, `_write`, `_pread64`, `_pwrite64`, and `_readv`, `_writev`, `_preadv`, `_pwritev` where the kernel has them | counted in the kernel | one row: a task, a syscall, a descriptor and a log2 bucket, with how many calls, their exact total, the bytes they returned and the errors |
| `io.fs` | `vfs_read`, `vfs_write`, regular files only | counted in the kernel | the same row keyed by `file:<superblock dev>+<ino>`, with the filesystem named from `mountinfo` |
| `io.fsync` | `do_fsync`, `vfs_fsync`, `vfs_fsync_range` | one record per sync | one sync: its file, its descriptor where it had one, `fsync` or `fdatasync`, the range, the return value and which function it was timed at |

`src/bpf/fentry.rs` is the attach kind and the timed pair the two histogram
sources share; `src/bpf/io_vfs.rs` and `src/bpf/io_fsync.rs` are the programs;
`src/collect/io/histogram.rs` is the collector half every kernel-built
histogram of this shape uses, `net.socket` included.

## The attach kind, in one paragraph

A tracing program is loaded against a function's type id in the kernel's own
BTF and attached through `BPF_RAW_TRACEPOINT_OPEN` with no name. Its context is
the function's arguments, one `u64` each, and an exit half gets the return
value in the slot after the last. The point is that the arguments are *typed*:
a `struct file *` arrives as a pointer the verifier follows, so
`file->f_inode->i_sb->s_dev` is three plain loads, each checked at load time
against the member it lands on -- the verifier's own log reads
`R1=ptr_file() ... R1=ptr_inode() ... R1=ptr_super_block()`. A load of the
wrong width is refused, which is why `i_mode` is read as a half-word
(`fentry::BPF_H`). The requirement `fentry:<function>` is answered from the
kernel's BTF and confirmed by loading two instructions against the function,
through the probe hook `capabilities.rs` already calls; every one of the eight
`fentry:` requirements the catalog carried read `unverified` before this.

## A function in BTF is not a function on the path

**This is the finding of this part, and it happened three times.** The catalog
named a function for each source, the function exists, it is in BTF, a program
loads and attaches against it -- and the calls being measured never pass
through it, because the compiler inlined it into its caller and kept an
out-of-line copy for someone else. Nothing fails. The source reports a quiet
machine.

| Named | What passes through it | Measured |
|---|---|---|
| `vfs_fsync_range` | `msync`, nfsd -- not `fsync(2)` | 25 syncs, **0** calls, machine-wide, no loss; 25 at `do_fsync` |
| `vfs_fsync` | nothing, here | 0 of the same 25 |
| `ksys_pread64`, `ksys_pwrite64` | the compat syscalls -- not `pread64(2)` | 300 `pwrite64`s, **0** calls; 500 `read`s through `ksys_read` found |
| `sock_sendmsg` | the kernel's own senders -- not `send(2)` | 2000 sends, **0** calls (`net.socket`, in `net.md`) |

So every source here is checked against a known workload before a function
name is trusted, and each was moved to where the calls actually are:

- **`io.syscall` is timed at the syscall table's own entry points**, the one
  layer that cannot be inlined into the dispatcher: it calls them from another
  compilation unit. They take a `struct pt_regs *` and read their arguments out
  of it, so the layer is x86-64's -- the requirement names `__x64_sys_read`,
  another architecture's probe reports it missing, and the collector refuses by
  name rather than reading another architecture's registers as x86-64's. The
  vectored four came with it for nothing: same prototype, descriptor in `di`.
- **`io.fsync` times all three of `do_fsync`, `vfs_fsync` and
  `vfs_fsync_range`**, because which one a sync reaches is the compiler's
  decision and differs between builds. The stamp records which function opened
  it; an inner call finds the thread stamped and leaves it alone, and an exit
  closes only its own function's stamp, so one sync is one event whether the
  three are nested or inlined. `do_fsync` has no `struct file *`, so there the
  file is found the way the kernel found it a moment before --
  `current->files->fdt->fd[fd]`, bounded by `max_fds` first and walked with
  probe reads, because an index into a table is not a typed load. The event
  says `via=do_fsync` so a reader can tell the walk from the typed argument.

## Where `io.fs` is attached, and why

The decision the seventh wave left. `io.fs` is timed at **`vfs_read` and
`vfs_write`, for regular files only**, for three reasons:

1. **It is the one place every filesystem passes.** The alternative the catalog
   carried was `tracepoint:ext4:*`, which answers for ext4 on a machine running
   btrfs -- this one -- and for nothing on xfs. A function below the VFS and
   above the filesystem is `->read_iter`, an indirect call with a different
   target per filesystem; choosing a target is choosing a filesystem again. The
   glob backend is gone from the entry, and the YAML says why beside it.
2. **The filesystem is named rather than chosen.** Each row carries the
   superblock's device, which `/proc/self/mountinfo` maps to a filesystem type,
   so a machine with btrfs and xfs mounted gets both, each labelled `fs=btrfs`
   or `fs=xfs`.
3. **A pipe is not a filesystem.** `vfs_read` also serves pipes, sockets and
   terminals, and a reader blocked on an empty pipe for ten seconds is not ten
   seconds of filesystem latency. The entry half reads `f_inode->i_mode` and
   times regular files only. That is also the separation the scenarios check
   between the two layers: a 300 ms pipe wait is in `io.syscall` and absent from
   `io.fs`, by construction.

## What the implementation had to decide

**Both histogram sources count in the kernel.** Every read on the machine
passes both layers, so a ring record per call would be a measurement whose
cost grows with the thing measured. What the kernel keeps is a row per
`(task, op, object, log2 bucket)` holding the count, the exact total, the bytes
the calls returned and the calls that failed -- `src/bpf/runqlat.rs`'s shape,
held once in `fentry::HistogramProgram` for the three sources that use it. An
event is a row: `lat_ns` is the bucket's low bound, which is what the emit
layer's own log2 bucketing maps back to the same bucket, and `count` and
`weight` say how many calls it stands for. Measured: `dd` reading 4 MiB in
4 KiB blocks came back as 8 rows, and the histogram the emit layer built from
them held `n=1025`, which is 1024 reads and the one that found the end.

**A row is created only if absent**, with `BPF_NOEXIST`. Two cpus closing the
first call of the same row at once would otherwise both insert, and the second
would overwrite the first's count. The loser is told `EEXIST` and adds to the
winner's row -- once, straight-line, because a jump back to the lookup is a
loop the verifier refuses (`infinite loop detected at insn 111` was the first
version's answer).

**A descriptor key names a number, and numbers are reused.** The interpreter
opens its own script as descriptor 3, closes it, and the workload's first
`open` gets descriptor 3 again. The first scenario found 596 reads on
descriptor 3 for a program that made 500. That is not a defect: a descriptor is
what the syscall layer has, and a row keyed by it is exactly as ambiguous as
the descriptor. The file is `io.fs`'s key, and the caveat on `io.syscall` says
so. The scenarios `dup2` their descriptors high enough that nothing else had
the number.

**On btrfs the file key is the superblock's device, and that is not
`st_dev`.** btrfs gives every subvolume its own anonymous device and reports
*that* from `stat`, while `inode->i_sb->s_dev` -- the only device a program at
the VFS can read without knowing the filesystem -- is one device for the whole
filesystem, and it is the one `mountinfo` prints. Measured under `/home`:
`stat` says `0:45`, the superblock and `mountinfo` say `0:28`. So the key's
device is written in `stat`'s encoding and named `sb_dev` on the event, the
scenario checks it against `mountinfo` rather than against `stat`, and the
catalog's caveat says the rest: inode numbers repeat across subvolumes, so on
btrfs the key names the filesystem and the inode, not always one file.

**A sync is streamed, not counted.** A database commits a few hundred times a
second, so a ring record per sync costs nothing worth measuring and keeps what a
histogram would throw away: which file, which range, which call -- the event a
slow commit can be named by, and joined on `file` to `io.fs`'s reads and writes
of the same file.

## Three losses nothing counted

The invariant is that a loss is counted and shown, always. Writing these
sources found three ways a call disappeared with nothing counting it, each
measured, and each now in `Losses::dropped` with its own reason.

**1. A drain racing the exit half's increment.** Every counting program in this
layer drained its map with lookup-and-delete while the programs kept writing
into it. An exit half that has looked a row up adds to it a few instructions
later; a drain that took the row in between has already copied it out, and the
increment lands in a freed element -- or, the map being preallocated, in
whatever key the element was handed to next. Measured: a hundred threads
reading a thousand files, **55192 counted and 44807 dropped, for 100000
reads** -- one gone without a trace. `HistogramProgram` now keeps two row maps
and a generation word: a drain empties the map that has been out of use for a
whole interval, then flips the word. The price is that a row reaches userspace
one interval late, and the event's `window_ns` is the interval it was counted
over rather than the time since the last drain. `sched.runqlat` and
`proc.syscall` drain the same way the first version did; the finding is filed
for them.

**2. The kernel skipping the exit half of a call in flight.** When *any* fentry
or fexit program is attached to or detached from a function -- by this tool, by
another run of it, by `bpftrace` -- the kernel rebuilds that function's
trampoline, and it patches the old one first so that calls still in flight
return *around* their exit programs. A 300 ms pipe read disappeared from
`io.syscall` the moment a second `omn` attached to `__x64_sys_read` for 30 ms,
**deterministically: 0 rows with the second tracer, 1 without**, and nothing
counted. It cannot be prevented from here; it can be found. The stamp the
entry half left is still in the pending map, so the entry half now stamps only
if absent and counts a stamp it finds as an exit that never ran, and at the end
a stamp whose thread no longer exists is counted the same way rather than as a
call in flight. The longest calls are the likeliest to be lost this way, which
is the worst kind of loss for a latency source, and the reason says so.
`tests/scenario_io.rs` has the second tracer on purpose.

**3. The kernel skipping a run of a preempted program.** A trampoline guards
each program with a per-cpu counter, and on a preemptible kernel -- this one is
`CONFIG_PREEMPT=y` -- a run preempted halfway makes the next hit of the same
program on that cpu skip it, counted only in `bpf_prog_info.recursion_misses`.
Every program's count is read, at the offset the kernel's own BTF gives the
field. **It was not observed on the fentry side**: zero in three runs of the
hundred-thread workload. It is read anyway, because it is the one loss of this
attach kind that nothing else could see -- and on the raw-tracepoint side, where
nothing read it, it turned out to be happening in every run of the suite; see
the next section.

## The block layer's one-percent tolerance was hiding a loss

`a_known_number_of_direct_reads_is_found_one_for_one` and
`the_tracer_and_the_kernels_own_counter_see_the_same_requests` are the block
layer's sharpest ground truth, from the seventh wave, and they flapped in this
wave's suite: short of 4096 by up to 90 %, with zero losses reported. Both pass
alone. Three different things were under that one symptom, and only the last is
a defect in the tool.

**1. The workload started before the tracer.** Both spawned `omn` beside the
test, slept 500 ms, and read. On a machine where thirty scenarios start at once
the attach took longer: the first request arrived 90 to 219 ms *after* the
workload's first read, timed on `CLOCK_MONOTONIC` on both sides. Fixed by
launching the workload -- `omn` attaches before it execs the program, and the
window is the program's lifetime, with no edge for a request to fall off. The
reads are made on a worker thread and the events are held to that thread's id,
so attributing to the process rather than the thread still fails; `--tid` on the
test's own thread was the old way of saying that.

**2. `O_DIRECT` is not "one read, one request" on btrfs.** With the edges gone
and the machine idle the count was 4095, every run. The partition's own
completed-read counter was the referee and agreed with the tracer: 4096 requests,
one of them a 16 KiB request flagged `RA` at the sector of the file's first page,
issued first. The program's first direct read fell back to a buffered one and
came through readahead. Separately, fifty direct reads of a page that *was*
resident produced 49 requests, and the counter moved by 49: btrfs answered one
from memory. The workload now evicts the file with `POSIX_FADV_DONTNEED` and
reports how much was still resident through `mincore` (the scenario skips, with
the number, if any was); and a readahead is accepted as the one other shape a
read may arrive in. Every request on the thread is a read, and no sector
appears twice.

**3. The kernel skipped the program, and nothing counted it.** With both of
those fixed, the scenarios were exact alone and still short by 2 to 23 inside
the suite -- six runs out of six -- with the missing pages scattered through the
run and the surrounding requests spaced as if the missing read had happened. A
system-wide trace of the same workload under the same load was exact, and so was
the launch under load or beside a loop attaching and detaching a second block
tracer; what the suite adds is thirty tracers at once. `recursion_misses` on the
two block programs said what it was: **766 and 1237 skipped runs** in one run
each. A raw tracepoint's probes run with preemption off, so the guard is found
taken only when an interrupt on that cpu issues or completes a request while the
program is still running, and that request's run is skipped whole.
`BlockProgram::skipped` now reads it and `io.latency`, `io.queue` and `io.size`
count it as dropped; so do `io.writeback` and the five `net` raw-tracepoint
sources. The count is the machine's -- the programs run on every request and
filter after -- so it bounds a target's share rather than equalling it, and the
reason says so.

**What the scenarios assert now.** Above 4096 is double counting, always. Below
it, the run must have reported at least the difference as lost; with nothing
reported, the count must be exact, and alone it is. Inside the suite the bound
is loose: a few hundred machine-wide skips against a shortfall of a few dozen
at most, so a collector that silently lost fewer than that many of this
thread's requests would pass there. It would not pass alone. Tightening it
needs a skip count per target, which the kernel does not keep. The
raw-tracepoint programs in the fourteen other files that load one do not read
the count yet; that is filed for their owners.

## What was measured

Every reference below is the workload's own account -- its loop bounds, what
`fstat` and `mountinfo` told it, its own clock around a call -- and never the
tool's.

- **Five hundred reads of 4096 bytes and three hundred `pwrite64`s of 1000,
  exactly, in both layers.** `io.syscall` finds `(500, 2048000)` on the read
  descriptor and `(300, 300000)` on the write one; `io.fs` finds the same two
  pairs on `file:<dev>+<ino>` for the inodes `fstat` gave, with the device
  `mountinfo` gives and `fs=btrfs`.
- **A 300 ms pipe wait**, timed by the program at 300,061,039 ns, is
  300,044,259 ns in `io.syscall` -- 17 us inside it -- and absent from `io.fs`.
- **Twenty `fsync`s, five `fdatasync`s and three `msync`s**: 28 events, all on
  the file the program named, 25 `via=do_fsync` carrying the program's
  descriptor and 3 `via=vfs_fsync_range`. A sync with 64 KiB to write and one
  with nothing to write sit three orders of magnitude apart (1 to 4 ms against
  about 1 us); the scenario asserts a factor of ten between the medians.
- **The loss path**: a hundred threads each `pread`ing a thousand files once is
  a hundred thousand distinct rows against a histogram that holds 32768 per
  drain. Measured, `io.syscall` dropped between 12698 and 13910 calls over four
  runs and `io.fs` 45031 in the one run it was read from; the run says every
  count is a lower bound, and the rows plus the drops are never fewer than the
  reads.

## What it costs, and what that did to the rest of the suite

**Attaching is the expensive part.** A 1 ms run of `io.syscall` takes 660 ms of
wall time -- sixteen programs, each attach and detach a trampoline rebuild --
against 387 ms for a raw-tracepoint source; `io.fs` and `io.fsync` take about
400. **Trampoline rebuilds are serialised machine-wide**: six `io.syscall` runs
started together took 2.2 to 2.4 s each. Once attached, a call costs the
trampoline and the program -- `io.fs` over `dd` charged 0.4 ms of cpu against
the target's 2.8 ms, a 14 % the tiny target makes large.

That serialisation is why `a_task_moved_between_two_cpus_is_counted_once_per_move`
in `tests/family_sched.rs` failed once in this wave's suite and passes alone:
it gives `omn` a fixed 800 ms to attach before it starts moving the task, and
with this stream's scenarios attaching beside it that is no longer enough. The
fix is in that file -- wait for the run to report it is attached, not for a
fixed delay -- and is filed rather than made. This stream's own sweeps push
their traffic for as long as the run lasts for the same reason.

## What this part cannot do

- **A cgroup target is refused** with exit 3, naming `bpf_get_current_cgroup_id`,
  like the rest of the family.
- **`io.syscall` is x86-64's.** On another architecture the requirement is
  missing and the resolver falls through to `bpf:raw_tp`, which has no
  collector.
- **`io_uring`, `sendfile`, `splice` and page faults on a mapping** pass none of
  these functions; every run says so in its notes.
- **`count` gets `raw` and `summary`, not `hist`.** A counted kernel histogram
  is still a histogram, but the mode x emit matrix says a count is one number
  per series, and the matrix is not this stream's.
- **The `bpf:kprobe` backends of all three have no collector.** On a kernel
  with BTF they would answer the same question with no type checking; nobody
  on such a kernel needs them, and `examples/net-not-built.yml` uses the
  equivalent `net` ones as its example of exit 3.
- **The catalog lost two keys it never produced.** `req` on all three and
  `file` on `io.syscall` were promises no backend kept; the keys now list what
  events carry.

## Sessions

- `examples/io-three-layers.yml` -- one program's reads at the syscall, in the
  VFS and at the device, with a pipe wait that only the first sees.
- `examples/io-syncs.yml` -- twenty syncs that wait for the device and five that
  do not, and three through a mapping, each named for the function that timed
  it.

Both need rows in `examples/README.md` and `GUIDE.md`, which are the
coordinator's; the test holding the indexes to the disk is red on this branch
until then, as the brief said it would be.

## Needs from another stream

Filed under `### From stream T` in `docs/open-findings.md`, each with the
signature wanted: a function lookup in `Btf` so `fentry.rs` can drop its own
index; the tracing load, a nameless raw-tracepoint open, a map update and a
program-info read in `syscall.rs`; the module lines in `bpf/mod.rs`; the
half-word width and three instructions in `insn.rs`; the drain race and the two
kernel-side losses for the owners of `runqlat.rs` and `proc_syscall.rs`; and the
fixed attach delay in `tests/family_sched.rs`.

---

# The page cache, through its own tracepoints

The ninth wave's stream P: `io.pagecache` and `io.readahead`, which had waited
only on a collector since the seventh wave landed the BTF walk into
`folio.mapping`. Both collect now, over one program pair in
`src/bpf/io_pagecache.rs`, read two ways by `src/collect/io/pagecache.rs`.

## What is collected

Two raw tracepoints on either side of a read:

```text
  read(2) ── filemap_read ── filemap_get_pages ─┬─ readahead ── __filemap_add_folio ──> mm_filemap_add_to_page_cache(folio)
                                                └──────────────────────────────────────> mm_filemap_get_pages(mapping, start, end)
```

- `mm_filemap_get_pages` is **what a read looked up**: the pages of
  `[start, end]`, found in the cache or brought in by the readahead the call
  set off. It is the denominator.
- `mm_filemap_add_to_page_cache` is **what the cache had to fill**: one folio
  entering one file's cache, in the context of the task whose read caused it
  (synchronous and asynchronous readahead both run in the reader).

Counted in the kernel, one row per `(task, file)` per 100 ms drain: pages looked
up, lookups, pages filled, the folios they came in, and pages created. The file
is `mapping->host->i_ino` and `->i_sb->s_dev`, the same key `io.fs` carries.

| Source | Value | Meaning |
|---|---|---|
| `io.pagecache` | `misses` | pages the cache had to fill for this task and file, on demand or read ahead |
| `io.readahead` | `ahead` | pages filled minus pages looked up, signed |

Every event carries both halves (`lookups`, `filled`, `created`, `folios`,
`lookup_calls`) whichever the value is. The series is keyed by file first,
because both questions are about a file and one file read by two processes is
one series.

## What the implementation had to decide

**1. `io.pagecache` needs both tracepoints, and its catalog entry said one.** It
required `mm_filemap_add_to_page_cache` alone, and a fill count alone is a miss
count with nothing to be a share of. It requires `mm_filemap_get_pages` too
now, which is the tracepoint `io.readahead` already named. On a kernel without
it both sources report the backend missing rather than half a ratio.

**2. A page a write appends is not a miss, and the kernel says which it is at
the instant.** Before this was told apart, a workload that only writes new
files read as a cache that never hits: an append adds a folio to the cache like
any read miss. The difference is visible when the folio is added, because a
write extends `i_size` only *after* its folio is in the cache and filled, and
readahead never reads past the end of the file. So the program reads the host
inode's `i_size`, and a folio whose index is at or past the page the file ends
in is counted as `created`; one inside the file as `filled`. Measured: a
megabyte appended in sixteen writes is **256 created, 0 filled, 0 looked up**.
What this cannot separate is a whole-page overwrite of a page that was not
cached: that is a fill with no read behind it, and is a miss here. The catalog
says so.

**3. `filemap_get_pages` reports a read's whole remaining range once per batch
of folios.** This was the first measurement and the one that changed the
design. `filemap_read` loops over batches of at most 31 folios, and each call
traces `[position, end of read]`, so a 1 MiB read reports `[0, 255]`, then
`[31, 255]`, then `[62, 255]`. Summing the ranges counted **3168 pages looked up
for a 1024-page file**, in 24 calls for four reads. A call from the same
thread, on the same mapping, with the same end and a start that has advanced is
the read it continues, and now adds nothing: the program keeps each thread's
last `(mapping, start, end, ts)` in a per-thread hash map. After it: **1024
pages in 4 lookups**, cold and warm.

That state cannot live in the drained row -- a drain landing between two batches
of one read would forget it and count the rest again -- and a plain hash map has
no eviction, so a thread that exits keeps its entry. Userspace sweeps entries
idle for more than a second, once a second. An entry the kernel refreshes
between the sweep's look and its removal is lost, and is counted with the
threads the map had no room for: both mean a later batch may be counted as a
lookup of its own, and the run then says `lookups` is an upper bound.
`bpf_task_storage` (stream V's, this wave) is the right home for this state: it
dies with the task and needs no sweep. It was not there to use.

**4. A lookup's range is clamped to the file.** The range's end is where the
read's buffer ends, so a 1 MiB read of a 100 KiB file reports 256 pages of which
25 exist, and the read at EOF that returns zero reports a range entirely past
the end. The program clamps to the pages `i_size` spans, and a lookup wholly
past the end counts nothing -- which is why the four 1 MiB reads of a 4 MiB file
are four lookups and not five.

**5. A folio is not a page.** btrfs on this kernel fills the cache with large
folios -- a cold 4 MiB read came in as 640 folios, a 16 MiB one as 856 -- so the
page count is read out of the folio: one page unless `PG_head` is set, and
`_nr_pages` then. `PG_head`'s bit is looked up in the running kernel's
`enum pageflags` rather than written down; the enum moves between releases.

**6. `ahead` is a signed difference, not a ratio.** A ratio of two window counts
does not add up: the window in which readahead filled a thousand pages and the
next in which the reader consumed them have ratios whose mean means nothing. A
difference does -- the sum of every window's `ahead` is the run's -- so `rate`
over a series is exact however the drains fell. It goes negative for a reader
served from the cache, and a log2 histogram has no bucket for a negative
number, so `io.readahead` no longer declares `hist`.

**7. Rows are stamped at their first event**, as finding 44 asks of every
bucketed source: each row records when it was created. The scenario checks it
against the workload's own `CLOCK_MONOTONIC`: every row of a file falls inside
the phase that read it.

## Ground truth

The workloads report the page cache's own state through `mincore(2)` -- a
syscall on a mapping, a different door into the kernel than a tracepoint, and
nothing the tool computed. Mapping a file and asking touches no page.

| Workload | Reference | Measured |
|---|---|---|
| 4 MiB read cold in 1 MiB reads | 1024 pages not resident before | 1024 filled, 1024 looked up in 4 lookups |
| the same, warm | 1024 resident before | 0 filled, 1024 looked up in 4 lookups |
| first 4 MiB of a cold 64 MiB file, 64 KiB reads | 1024 read, resident after settling | 2816 filled, **+1792 ahead** |
| 256 random pages of a cold 64 MiB file | 256 read | 256 filled, **0 ahead** |
| 1 MiB appended in 16 writes | 256 pages written | 256 created, 0 missed |
| `examples/io-cold-then-warm.yml` | 4096-page file read twice | 4096 misses; warm window exactly -4096 |

The sequential-against-random case is asserted as a separation (sequential
ahead > 0, random at most an eighth of it), not a size: how far the kernel reads
ahead is its window, which starts at `read_ahead_kb` -- 128 KiB here -- and grows
while the reader keeps up.

## The cold lookups are a floor, and the kernel is why

The first version of these scenarios asserted a cold read's lookups exactly, and
failed about one run in five while the rest of the file ran beside it: 1023
pages of 1024 in 4 lookups; a session's cold window +48 where it should be 0;
88 and 96 in others. No loss was reported, the warm windows were always exact,
and twelve runs under an artificial load of a hundred reading threads were all
exact. The number itself was the clue: a shortfall of 1, 48, 88 or 96 pages is
the size of a *first folio batch*, not of a read.

`filemap_get_pages` fires its tracepoint on one return only. When the last folio
of a batch is still locked under readahead I/O, `filemap_update_page` waits for
it, returns `AOP_TRUNCATED_PAGE`, and `filemap_get_pages` drops that folio and
returns the rest of the batch -- successfully, and without the tracepoint. The
read carries on; its next call is then the first one this program sees, it does
not continue anything it knows, and it is counted from where it starts. The
first batch was looked up unseen.

So: **warm lookups are exact** (no folio is locked, no early return), **cold
lookups are a floor**, short by at most a read's first batch, and **random
single-page reads are exact** either way (a batch of one folio cannot take that
return). The scenarios assert exactly that, and both catalog entries carry it as
a caveat. On a cold sequential read `ahead` can be a few pages high for the
same reason. The backend that would make it exact is `fentry:filemap_read`,
whose `kiocb` and `iov_iter` give a read's range once, at entry; this stream
did not write it.

## The loss path

A hundred threads each reading a thousand one-byte files is a hundred thousand
`(task, file)` rows against a map of 16384 per drain. Measured: **100000 reads =
65483 counted + 34520 dropped** (the three extra are fills, and one run of the
programs the kernel skipped), and the run says its totals are a lower bound. The
kernel skipping a run -- `recursion_misses` on the program -- is read and
counted as a loss too: it happened thirteen times in one such run.

## What it costs

About **400 ns per read** in the reader's own time: 1024 warm 64 KiB reads took
4.6 ms bare and 5.0 ms with `io.pagecache` attached. The collector's own cpu is
0.4 to 0.7 ms per run, a drain every 100 ms walking one row per `(task, file)`.

## What this part cannot do

- **A page a fault brings in is a miss with no lookup.** `filemap_fault` does
  not pass through `filemap_get_pages`, so a program that reads through `mmap`
  shows misses and no lookups. `mm_filemap_fault` and `mm_filemap_map_pages`
  would give the lookup side, and counting a fault-around's sixteen pages as
  sixteen lookups is a decision for whoever does it.
- **Whether a page read ahead was used before it was evicted.** `ahead` says
  how far readahead ran in front of the reader, not whether the reader caught
  up. That needs `mm_filemap_delete_from_page_cache` and a per-page record of
  what was read ahead and not yet looked up, which is a different program.
- **A cgroup target.** `io.pagecache` declares one; this build narrows by
  thread group and thread only, and refuses a cgroup with exit 3 naming
  `bpf_get_current_cgroup_id`, as `io.fs` does.
- **tmpfs and shmem.** Neither passes `filemap_get_pages` or
  `__filemap_add_folio`, so both sources are silent on them -- which is why every
  scenario writes under the build directory.

## Declared overstep

**Four lines in `src/bpf/mod.rs`**, which no stream owns this wave: the `mod
io_pagecache;` declaration beside `mod io;`, and one `pub use` of its three
types and the sweep constant. A new file under `src/bpf/` cannot be compiled
without them, and `attach.rs`'s `#[path]` precedent is a file nobody owns
either. The lines touch nothing stream V's `request*.rs` declarations would sit
beside.

## Sessions

- `examples/io-cold-then-warm.yml` -- one file read twice, cold and warm, under
  both sources: 4096 misses, and -4096 ahead.
- `examples/io-readahead-patterns.yml` -- readahead in front of a sequential
  reader, and not in front of a random one.

The rows for `examples/README.md` and `GUIDE.md`, the same in both:

```
| `io-cold-then-warm.yml` | One file read cold and then warm: every page missed once, and the second read served from the cache. |
| `io-readahead-patterns.yml` | Readahead in front of a sequential reader, and not in front of a random one. |
```

`GUIDE.md` line 130 still names `io.readahead` among the sources left without a
collector.

## Tests red on this branch, and why

- `tests/cli.rs` `the_session_indexes_hold_exactly_the_sessions_on_disk`: the
  two sessions above, until the coordinator adds the rows.
- `tests/cli.rs` `a_valid_request_with_no_collector_names_the_ones_that_exist`:
  it looks for a source with no collector that declares `trace`, `raw` and a
  system target, and `io.pagecache` and `io.readahead` were the last two. What
  is left uncollected declares other modes. The change it needs is in
  `docs/open-findings.md` under stream P.

The one `make check` before the commit, then `cargo test --workspace
--no-fail-fast`: 1545 passed and 5 failed. Beside the two above,
`tests/aggregation.rs` `two_emits_of_one_request_describe_the_same_events` is
the one-ulp float comparison stream J filed, red on `main` too;
`tests/cross_compute.rs` `three_states_at_once_still_match_the_kernel_and_their_residue_is_printed`
passed alone; and `tests/scenario_mem.rs`
`both_huge_page_backends_read_the_same_promotion_counter`, in the tally of
finding 43's neighbours, failed alone as well, with the two backends 30 huge
pages apart on every one of five rounds (`[(377, 347, 3), ...]`). It reads
machine-wide THP counters and nothing on this branch touches `mem`; it is
reported here for stream B rather than fixed, and it is the one of the five
worth watching, because it failed twice in a row.

## Needs from another stream

Under `## Needs from another stream — the ninth wave, stream P` in
`docs/open-findings.md`: the `cli.rs` test above; `map_update` out of
`#[cfg(test)]` in `syscall.rs`, so the sweep can put back an entry the kernel
refreshed; and, for when `bpf_task_storage` lands, the per-thread map it would
replace.
