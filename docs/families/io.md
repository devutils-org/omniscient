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

Five sources, all through procfs, all system-wide. They are free, they are
exact, and they say nothing about who caused the I/O -- attribution is the eBPF
backends' job and arrives with that layer.

| Source | Backend | What an event carries |
|---|---|---|
| `io.bytes` | `fs:proc` (`/proc/diskstats`) | bytes the devices moved during the tick, split into reads and writes |
| `io.iops` | `fs:proc` | operations the devices completed, with discards and flushes counted separately |
| `io.merges` | `fs:proc` | requests the block layer combined |
| `io.inflight` | `fs:proc` | requests outstanding at the instant of the read |
| `io.pressure` | `fs:proc` (`/proc/pressure/io`) | the share of the tick during which something was stalled on I/O |

Nine more (`latency`, `queue`, `size`, `syscall`, `fs`, `pagecache`,
`readahead`, `writeback`, `fsync`) are block or VFS tracepoint work and wait for
the eBPF layer. `io.bytes` and `io.pressure` also declare a `fs:cgroup` backend
-- `io.stat` and `io.pressure` under a cgroup -- which waits for a cgroup
target.

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
