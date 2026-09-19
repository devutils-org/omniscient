# Scenarios — `io`

What is worth measuring in the block layer with this build, and how you know
the answer is right.

Five of this family's fourteen sources have a collector, and all five read the
same file: `/proc/diskstats`. That single fact decides everything below. These
are the **device's** counters, kept **per device** for the **whole machine**,
with **no record of who asked** — so every scenario here is a machine-wide one,
every ground truth is a quantity of I/O pushed through a real device on
purpose, and the question every reader arrives with, *which of these bytes were
mine*, is answered with a refusal rather than with a number
(`examples/io-per-process.yml`).

| Source | Modes served here | What it is |
|---|---|---|
| `io.bytes` | `count`, `poll` | bytes the devices moved |
| `io.iops` | `count`, `poll` | requests the devices completed |
| `io.merges` | `count` | requests the block layer coalesced |
| `io.inflight` | `poll`, `snapshot` | queue depth, read at an instant |
| `io.pressure` | `poll`, `snapshot` | PSI: share of a window stalled on I/O |

The sessions are in `examples/io-*.yml` and the checks in
`tests/scenario_io.rs`. Everything quoted below was produced on the development
machine — an NVMe disk, `max_sectors_kb` 128, the `none` scheduler, btrfs
without compression — and every number a scenario asserts is derived from the
workload or from sysfs, never from the tool.

---

## The ground truth, in one paragraph

Two workloads, both with a property that is true before the tool is started.

**64 MiB written and `fsync`ed must reach the platter**, because `fsync` does
not return until it has. **64 MiB read after the page cache was told to forget
the file must come off it**, because `POSIX_FADV_DONTNEED` drops the clean
pages of a file its caller owns. The first gives a floor — the filesystem
writes its own metadata alongside, so the device moves a little more. The
second gives near-equality: readahead cannot read past the end of a file, so on
a machine doing nothing else the device moves exactly the file's length.

Neither number is a plausible constant, which is the point. A collector that
reported sectors as bytes would be out by 512. One that summed every line of
`/proc/diskstats` would be out by two or three, since a partition's I/O is
counted again on the disk holding it. One that reported the counter instead of
its increase would be out by the machine's whole uptime. Each of those is a
real mistake, and each fails the check below.

```console
$ omn -f examples/io-known-volume.yml        # while a 64 MiB file is read cold
volume  via fs:proc  (io.bytes)
  2 events over 2.00s, stopped because the requested duration elapsed
  no loss: every read succeeded and no tick ran late
  summary
    dev:253+0 n=1     min 0 B  mean 0 B  max 0 B  last 0 B
    dev:259+0 n=1     min 64.0 MiB  mean 64.0 MiB  max 64.0 MiB  last 64.0 MiB
operations  via fs:proc  (io.iops)
  summary
    dev:259+0 n=1     min 513 count  mean 513 count  max 513 count  last 513 count
merges  via fs:proc  (io.merges)
  summary
    dev:259+0 n=1     min 0 count  mean 0 count  max 0 count  last 0 count
```

64.0 MiB is the file. 513 is `67108864 / 131072` plus one, the device's own
`max_sectors_kb` being what decides how many requests a transfer takes — that
divisor is read out of sysfs by the test, not assumed. The zero is the third
check and the easiest one to fake: this device's scheduler is `none`, so there
is nothing to coalesce and the right answer is exactly zero. A collector
returning a plausible number would pass every test that only asks for a number.

---

## The scenarios

### 1. How much did the devices move, and in how many requests?

`examples/io-known-volume.yml` — `io.bytes`, `io.iops` and `io.merges` counted
over one window.

- **Workload**: either of the two above. The test drives both, in
  `a_known_volume_of_writes_reaches_the_device_and_the_session_finds_it` and
  `a_cold_read_of_a_known_file_is_measured_against_the_bytes_it_had_to_move`.
- **Check**: the bytes reported for the device backing the file are at least
  the workload's 67108864, and at most what the kernel's own counters moved
  over a window enclosing the session's — read by the test itself, before and
  after, so the number is bracketed from both sides and neither side is the
  tool's. The operation count is at least `bytes / max_sectors_kb`. The merge
  count is zero where the scheduler cannot merge.
- **Why it is one session and not three**: volume alone cannot tell a few large
  transfers from many small ones, the operation count alone cannot tell a busy
  device from a fast one, and the quotient of the two is the mean request size,
  which is the number that says which you have. Counted over one window, they
  are commensurable; measured in three runs they are three moments.

Finding the device is part of the scenario. `stat` does not give it: btrfs
hands every subvolume an anonymous device number that appears in no other
file, so `st_dev` cannot be matched against `/proc/diskstats`. The mount source
can, and a partition then has to be resolved to the disk below it, because that
is the one whose counters are its own.

### 2. What are the disks doing, and was anything waiting?

`examples/io-device-load.yml` — the same three quantities polled at 20 ms, plus
the one that is not a counter.

- **Workload**: 64 MiB written and synced, which keeps tens of requests in
  flight for a couple of hundred milliseconds.
- **Check**: `a_burst_of_writes_shows_up_as_a_queue_on_the_device_that_took_it`
  asserts the maximum depth on that device is at least one, that the polled
  volume series adds up to at least the 64 MiB the workload synced — the second
  road to the number scenario 1 counts — and that a two-second window polled
  every 20 ms produced about a hundred points rather than a handful.
- **Why the interval is short**: queue depth is a gauge. A queue that fills and
  drains between two reads leaves no trace at all, so at 1 s this scenario is a
  coin toss and at 20 ms it is not. The series then describes the device rather
  than the sampling, and that is the one way this source lies.

### 3. Is the disk the bottleneck, or merely busy?

`examples/io-pressure.yml` — PSI beside volume.

- **Check**: `pressure_is_a_share_of_its_own_window_and_full_never_exceeds_some`
  asserts, on every tick of the run, that the share is between nothing and all
  of the window, that `full` never exceeds `some` — it is the share during
  which *every* runnable task was stalled, a subset of the share during which
  one was — and that the ratio is exactly the stall over the interval that
  produced it.
- **What it cannot check here**: whose stall it is. See below.

### 4. A poll that cannot keep up

`examples/io-late-ticks.yml` — `io.bytes` polled every 10 µs.

- **Workload property**: the machine's own. One read of `/proc/diskstats` costs
  about 21 µs here — the kernel formats one line per block device on every
  read, and this machine has ten — and the test measures that itself, taking
  the median of a hundred reads, before asserting anything. A tick shorter than
  one read cannot be met, not sometimes but never.
- **Check**: every round of reads is counted as late (the number of rounds is
  the event count over the number of devices), the count reaches the text
  output, and the run does not say `no loss`.

```console
$ omn -f examples/io-late-ticks.yml
too fast to read  via fs:proc  (io.bytes)
  4372 events over 200.02ms, stopped because the requested duration elapsed
  LOSS: 0 dropped, 0 failed reads, 2186 late ticks
```

This is the family's honest loss path, and it is worth being clear about what
is lost: not events, but the spacing the series claims. Every point is a true
increase over a true interval; the interval is not the one that was asked for,
and a run that reports `poll@1ms` while sampling every 1.3 ms is a run whose
rates are quietly out by a third.

### 5. Counters that lie

`examples/io-substituted-counters.yml` — the ordinary session, pointed at a
`/proc/diskstats` written by hand and bind-mounted over the real one inside an
unprivileged user namespace.

```console
$ printf ' 259 0 nvme0n1 10 0 2048 0 0 0 0 0 0 0 0\n' > /tmp/fake.diskstats
$ unshare -Urm sh -c 'mount --bind /tmp/fake.diskstats /proc/diskstats &&
      exec omn -f examples/io-substituted-counters.yml'
```

Three of the four cases cannot be produced on a running machine — a disk does
not vanish because a test would like it to — and the fourth is the only way to
check the arithmetic exactly. It needs no root, no hardware and no loopback:
one user namespace and one mount namespace, both unprivileged.

| What the file does | What must come back |
|---|---|
| `sectors read` rises by 2048 | one point of exactly 1048576 bytes |
| a line for a device with no `/sys/class/block` entry | left out of the total, **counted** as a loss, with the reason |
| a counter goes backwards | the interval is unmeasurable, **counted**, and named |
| a device appears mid-run | its first interval is dropped and **named** — and not counted; see Defects 4 |

`substituted_counters_are_converted_exactly_and_their_gaps_are_named` drives
all four in one run, rewriting the file between ticks;
`a_line_left_out_of_the_total_reaches_the_reader_at_a_terminal` runs the second
case again in text, because what the structured output carries and what a
person sees are not the same thing here.

The name in the file has to be a device this machine actually has, or the first
case becomes the second: the collector asks sysfs
whether a device's counters are its own rather than reading its name, which is
the only thing that works — `nvme0n1p6` looks like a partition and is one,
`dm-0` looks like a disk and is stacked on others, `zram0` looks synthetic and
is genuinely its own device.

### 6. The capability sweep

`every_mode_and_emit_the_catalog_offers_is_served_or_refused_by_name` drives
every mode × emit pair the catalog declares for the five collected sources,
built from the catalog rather than from a list in the test, so a mode or emit
added tomorrow is swept tomorrow. Thirty-two pairs on this build:

| Outcome | Pairs | What it means |
|---|---|---|
| served | 20 | ran, and rendered the emit that was asked for |
| refused, exit 2 | 8 | the mode and the emit cannot mean anything together, and the message says what the mode does instead |
| refused, exit 1 | 4 | every `trace` pair: the catalog declares the mode, the resolver falls back to `fs:proc`, and the collector refuses at attach — see Defects 1 |

`every_output_format_either_carries_the_series_or_refuses_it` does the same
along the format axis: `json`, `yaml`, `toml`, `ndjson`, `speedscope` and
`chrome` all carry an `io.inflight` timeline, and `folded` and `pprof` refuse
it by name, since they are stack formats and a timeline has no stacks. That is
the quietest of the format lies — a file a flamegraph tool would read as an
empty profile rather than as an error — and it does not happen.

Filters are the third axis.
`a_filter_the_backend_cannot_carry_names_the_backend_that_can` checks both
halves: `--filter "comm == 'dd'"` is refused, naming `bpf:tp` as the backend
that carries `comm`, and `--filter "dev == ..."` narrows the series and reports
what the predicate threw away — `kept 0 of 6 events, evaluated after reading`
— so that an empty result and a quiet machine do not look the same.

### 7. The refusals

Three answers that must not be confused, and
`each_refusal_names_the_thing_that_is_actually_missing` provokes each:

| Session or command | It means | Exit |
|---|---|---|
| `omn io.inflight --system -m count -e rate` | a queue depth counts over no window; the request cannot mean anything | 2 |
| `omn -f examples/io-per-process.yml` | valid, serveable here, written for nobody: no collector on `bpf:tp` | 3 |
| hiding `/proc/pressure` behind a tmpfs in a namespace | this machine cannot serve `io.pressure` at all | 2, and should be 1 — Defects 3 |

The per-process refusal is worth its own session because of what the
alternative would have been. `/proc/diskstats` is readable, free and right
there; answering a `--pid` from it would report the whole machine's I/O under
one process's name. That is the worst thing this tool could do, and a wrong
number that looks right survives every review.

---

## Workloads

Phase two reuses these. Both are in `tests/scenario_io.rs`, both take a path on
a real block device, and both are also written out as shell one-liners in the
sessions that use them so a reader can run them by hand.

| Workload | Guarantees | Costs |
|---|---|---|
| `write_and_sync` — 64 MiB of incompressible bytes, then `sync_all` | at least 67108864 bytes reach the device, plus the filesystem's own metadata (0.3–1 MiB here); tens of requests in flight for ~200 ms; the writing task blocked on I/O for 55–68 ms of that, measured against `/proc/pressure/io` | ~250 ms |
| `cold_read` — `POSIX_FADV_DONTNEED`, then read the file whole | exactly 67108864 bytes come off the device on an otherwise quiet machine, in `67108864 / max_sectors_kb` requests | ~150 ms |

`write_and_sync` is the one **P3 (the I/O path)** and **P4 (crossed families)**
will want. It spends most of its time blocked on the device, so the same run
produces `sched.offcpu` events in the `uninterruptible` state at the moment
`io.pressure` and `io.bytes` see the writeback — which is the crossing P3 has
to make until the eBPF backends give `io` a per-request key. It is not a cpu
workload: a program that is half on-cpu and half blocked has to come from
another stream, and this one is barely on-cpu at all.

Neither is run where it would prove nothing. A build directory on tmpfs reaches
no device, and a filesystem mounted with compression turns a known number of
bytes written into an unknown number stored; both are checked before the
workload starts, and the scenario stops and names the one that is in the way
rather than measuring nothing.

---

## What `io` contributes to a crossing, and on which key

One line, as `BRIEF.md` asks for: **`io` knows what the device actually did,
which no other family can see** — the page-cache hits that never reached it,
the readahead and journal writes that did, and the queue that requests waited
in. What it cannot say is who caused any of it.

The key it carries today is **`dev`**, on every event of all five sources, as
`dev:<major>+<minor>`. That is enough to join `io` to anything else keyed by
device and to nothing else. The keys that would matter for phase two —
`blk:rq@open_ts` for `scoped_eq`, and `task` — are declared by the catalog and
produced by no backend in this build; both arrive with the eBPF layer. Until
then the crossing P3 describes has to go the other way round, from
`sched.offcpu` in the `uninterruptible` state to `io.pressure` over the same
window, with the window as the only thing the two share.

---

## What cannot be verified here

Stated rather than worked around, because a scenario that hides its limit is
worth less than one that names it.

**Whose stall it is.** PSI is kept for the whole machine. On a machine where
anything else is reading or writing, the share `io.pressure` reports is
everyone's, and no workload can be told apart from the rest by that number
alone. The scenario therefore measures an idle window first and only compares
when the baseline is low enough to mean something; on the development machine,
shared with other builds, it usually is not, and the test says so rather than
passing quietly:

```
skipped the comparison: this machine was already stalled on I/O 88 % of an idle
window, so a workload's own contribution cannot be told apart from the rest
```

The fix is not a better test. It is `fs:cgroup`, which reads the same numbers
per cgroup and is declared, `equivalent: true`, with no collector — the one
place in this family where a backend comparison would be worth running.

**Merges above zero.** `io.merges` cannot be driven anywhere but zero on this
machine, and not for want of trying: the device is multi-queue with the `none`
scheduler, and the filesystem submits requests already at `max_sectors_kb`, so
there is nothing adjacent to coalesce. Zero is the right answer and the
scenario checks it *is* zero, which is a real check — but a machine with
`mq-deadline` or `bfq` and a workload of many small adjacent reads would
exercise the other half, and nothing here does. The test branches on the
scheduler and says which of the two checks it ran.

**A backend comparison.** Class B has nothing to compare in this family: each
of the five sources has exactly one implemented backend, so
`--backend force:` cannot put two roads to one number in one window, and the
self-benchmark in `src/main.rs` needs two distinct backends before it prints
anything. What the scenarios do instead is the substitute `BRIEF.md`
prescribes — force every declared-and-unwritten backend and check the refusal
names it and exits 3 —
`forcing_a_backend_this_build_has_not_written_names_it_and_exits_three`. The
comparison that will be worth writing first, once there is a second collector,
is `io.bytes` through `bpf:tp` against `fs:proc` over one window: they measure
the same bytes from opposite ends of the block layer, and the gap between them
is readahead and metadata rather than error.

**Anything above the block layer, and anything per request.** Nine of this
family's fourteen sources have no collector: request latency and queueing time
and request size, which need the block tracepoints, and syscall latency,
filesystem latency, page-cache hits, readahead, writeback and `fsync`, which
need `fentry` or tracepoints above the block layer. So the three layers this
family exists to keep apart cannot yet be compared. The gap between what the
application asked for and what the device did *is* the diagnosis, and today
only one side of it can be measured.

**Two-sided bounds under load.** Every byte check here is a floor plus the
kernel's own counters as a ceiling, never an equality, because the machine's
other I/O is in the number. The equality holds on a quiet machine and is quoted
above; it is not asserted, because a test that fails when somebody else starts
a build is a test that will be deleted.

---

## Defects found

Nine, none of them worked around. Each is reproducible with the session or
command line given. One — 9, the loss headline — is fixed; its reproduction is
left in place with what the run prints now beside it.

### 1. A declared mode, refused at attach, under the exit code for a failure

> **Fixed** (stream A). Exit 3. The sequence described below is unchanged -- the
> resolver still passes over `bpf:tp` and selects `fs:proc` for a `trace` request,
> which is finding 1 of `docs/open-findings.md` on the mode axis -- but the
> refusal is raised by the factory, before a device has been touched, so it is
> read as this build's gap. Asserted by
> `each_refusal_names_the_thing_that_is_actually_missing`.

```console
$ omn io.bytes --system -m trace -e topk -d 1s
error: cannot start collecting `io.bytes`: `/proc/diskstats` holds the block layer's
counters, not its events: `io.bytes` per request needs the tracepoint backend, which
arrives with the eBPF layer
$ echo $?
1
```

The message is right and names the mechanism that would serve it. The exit code
is 1, which is what a run that failed returns — out of memory, a target that
disappeared, a counter that would not open. This is a request that is valid,
that this machine can serve, and that this *build* does not implement, which is
exit 3. `docs/open-findings.md` §5 records exactly this ("A mode a collector
cannot serve returns `CollectError::Start` and exits 1"); what the sweep adds
is how much of the family's declared surface it covers. `io.bytes` is the only
collected source here that declares `trace`, and all four of its emits go this
way — four of the thirty-two combinations the catalog offers, and every one
that names the mode the eBPF backend exists for.

The sequence behind it is worth naming, because it is what the capability sweep
is for: the resolver passes over `bpf:tp` as unimplemented, selects `fs:proc`
for a `trace` request because modes are declared per entry and not per backend,
and `fs:proc` then refuses at attach. Nothing along the way is wrong on its own.

A consequence: `io.bytes --emit topk` is unreachable in this build in every
mode. `count` and `poll` reject it as incompatible with the mode, and `trace`,
where it would live, ends as above.

### 2. The mode's hint offers an emit the source refuses

**Fixed** by stream B of the fifth wave; see `docs/fixes/catalog.md`. The hint
is now the intersection of the matrix and the entry, so it can only ever name
emits this source declares:

```console
$ omn io.bytes --system -m count -e timeline
error: [io.bytes #1] mode `count` cannot produce `timeline`
  hint: in `count` mode `io.bytes` emits rate, summary, raw

$ omn io.bytes --system -m count -e raw -d 1s
  raw
    88331025716875  io.bytes  dev:259+0  bytes=81920 read_bytes=0 write_bytes=81920 ...
```

Both halves moved: the hint narrowed, and `raw` is declared, which is defect 7
below. `the_mode_hint_offers_only_what_the_source_declares` replaces the
scenario named at the end of this entry, and follows the hint rather than only
reading it. What it used to say:

```console
$ omn io.bytes --system -m count -e timeline
error: [io.bytes #1] mode `count` cannot produce `timeline`
  hint: in `count` mode the valid emits are rate, summary, raw

$ omn io.bytes --system -m count -e raw
error: [io.bytes #1] `io.bytes` cannot emit `raw`
  hint: this source emits rate, summary, timeline, topk
```

The first hint lists what a `count` mode can produce in the abstract; the
second refuses one of them for this source. A reader who follows the first hint
gets the second refusal. The mode × emit matrix and the source's own `emits`
are both right; nothing intersects them before printing advice.
`the_mode_hint_offers_an_emit_the_source_refuses` held both messages so that
the day the hint was narrowed the scenario would say so. It did, and it is now
`the_mode_hint_offers_only_what_the_source_declares`.

### 3. "No viable backend" and "this cannot mean anything" share an exit code

> **Fixed** (stream A). Exit 1, asserted by
> `a_machine_that_cannot_serve_pressure_says_which_file_it_wanted`.

```console
$ unshare -Urm sh -c 'mount -t tmpfs none /proc/pressure &&
      exec omn io.pressure --system -m poll@100ms -d 200ms -e timeline'
error: `io.pressure` has no viable backend on this machine:
  fs:proc: /proc/pressure/io: missing: /proc/pressure/io does not exist
  fs:cgroup: `fs:cgroup` cannot attribute per system: it would answer with a number
             scoped to cgroup, container
  hint: the request is valid; it is this machine that cannot serve it.
$ echo $?
2
```

The message is exemplary: it names the file, names the backend, and says the
request itself is fine. The exit code is 2, the code for a request that cannot
mean anything — `BRIEF.md`'s table gives 1 for this case, and `src/main.rs`
returns `EXIT_REJECTED` for every resolver error. A caller that scripts around
these three answers cannot tell "fix your request" from "run this somewhere
else".

### 4. A dropped interval, noted and not counted, in a run reporting `no loss`

```console
$ # a device appears in /proc/diskstats mid-run, under a substituted file
io.bytes  via fs:proc  (io.bytes)
  7 events over 500.48ms, stopped because the requested duration elapsed
  no loss: every read succeeded and no tick ran late
```

The collector records a reason — *"`zram0` appeared during the run, so this
interval has nothing to measure its counters against"* — and increments
nothing. `Losses::is_clean` is therefore true, the text renderer prints `no
loss`, and the reason itself never reaches the output, because reasons are only
printed under a non-zero count. An interval of one device's I/O is missing from
the run and the run says it is complete.

This is the standing invariant — *losses are counted and surfaced to the user,
always* — failing in the one place that is hardest to notice. The sibling case
one line further down in the same function, a counter that went backwards, does
increment `undecoded`, which is what makes the omission look like an oversight
rather than a decision. The scenario asserts the reason is recorded and
deliberately does not assert the count, so that fixing this does not have to
fight the test.

### 5. Loss reasons are keyed by the measured duration, so they cannot aggregate

> **Fixed** (stream A). One reason carrying the count it explains, with the
> slowest and the mean beside it in `losses.overrun`. Asserted by
> `a_poll_faster_than_the_file_can_be_read_counts_every_late_tick`.

```console
$ omn -f examples/io-late-ticks.yml | wc -l
1346
$ omn -f examples/io-late-ticks.yml | head -4
too fast to read  via fs:proc  (io.bytes)
  4372 events over 200.02ms, stopped because the requested duration elapsed
  LOSS: 0 dropped, 0 failed reads, 2186 late ticks
    1x a round of reads took 100181ns, longer than the 10000ns tick
```

`Losses::reasons` is a map from reason to how often it occurred, and every late
tick builds a reason string containing its own duration in nanoseconds. A
two-hundred-millisecond run therefore produces some thirteen hundred distinct
reasons, almost all of them seen once, and prints a line for each — the count
they explain is on line three and the thirteen hundred lines after it are
noise. The same map grows without bound for the length of a run, in memory and
in every structured output. The duration belongs in a summary — slowest,
median, how many — not in the key.

### 6. Request order in a session decides another request's sampling rate

> **Fixed** (stream A). Each request is driven by its own `Pace`, from its own
> mode and its own collector: a collector that declares a tick keeps it, and one
> that declares none is read at the cadence the request named. This closes the
> other side of it too -- §24 of `docs/open-findings.md`. Asserted by
> `request_order_does_not_decide_another_requests_rate`.

```console
$ cat counting-first.yml           # io.bytes count, then io.inflight poll@100ms
$ omn -f counting-first.yml | grep "events over"
  2 events over 1.00s, stopped because the requested duration elapsed
  4 events over 1.00s, stopped because the requested duration elapsed

$ cat polling-first.yml            # the same two requests, the other way round
$ omn -f polling-first.yml | grep "events over"
  22 events over 1.00s, stopped because the requested duration elapsed
  2 events over 1.00s, stopped because the requested duration elapsed
```

The polled request asked for 100 ms in both files. It got 100 ms when it was
written first and 1 s when it was written second — two points instead of
eleven, with no warning and no loss recorded. `run_all` takes one interval for
the whole run, and where no collector declares a tick of its own it falls back
to `resolved[0].request.mode`: the mode of whichever request happens to be
first. A counting request's fallback is the default poll interval, one second.

`docs/open-findings.md` §24 has the general shape of this — a session polls
every collector at the pace of its fastest one — and this is the same seam seen
from the other side: not a fast collector dragging a slow one, but a request
that has no cadence of its own imposing one on a request that does. The fix
proposed there, asking each collector at `max(its own tick, the interval its
request named)`, would close both. Both io sessions that poll more than one
source use the same interval on every request, which is a way of not stepping
on it rather than a fix.

### 7. `io.bytes` measures the read/write split and no emit can show it

The collector puts `read_bytes`, `write_bytes`, `sector_bytes` and, where the
kernel reports them, `discarded_bytes` on every event. The catalog declared
`emits: [rate, summary, timeline, topk]` for `io.bytes` — and none of those
carries an event's attributes. `raw`, which does, was not declared, and was
refused by name.

**Fixed** by stream B of the fifth wave; see `docs/fixes/catalog.md`. `raw` is
declared on all three entries, which needed no new mode: it is producible from
`count`, `poll` and `trace`, which is every mode these sources have. The split
is now reachable in each of them, and each entry carries a caveat naming it.

What pins it is ground truth rather than shape: the scenario
`the_split_a_write_workload_made_is_reported_as_writes` runs the same `fsync`ed
64 MiB past `io.bytes -m count -e raw`, requires `write_bytes` to carry it, caps
each of the two columns with the same column of `/proc/diskstats` over a window
enclosing the run, and requires them to add up to the total the aggregating
emits answer with. That is the check the total could not make.

Its first form asserted that a write workload reads nothing, and that measured
the suite rather than the tool: run beside the cold-read scenario two entries
above, which pulls 64 MiB off the same device, it failed with exactly 67108864
bytes in the read column — the neighbour's. The brackets are what survive a
shared machine, and on a quiet one the read cap is near zero and says the same
thing.

So the first question anybody asks of a volume — *was that reads or writes?* —
is measured, attached to the event, and unreachable. It costs the scenarios
here something concrete: the write workload and the read workload can only be
checked against the total, and a collector that credited every write as a read
would pass both. The same holds for `io.iops`, whose `ops` total hides
`reads`, `writes`, `discards` and `flushes`, and for `io.merges`, whose
`merges` total hides `read_merges` and `write_merges`.
`docs/open-findings.md` §5 records that the `cpu` thread added `count` → `raw`
to the mode × emit matrix for exactly this reason, on the grounds that a
counted window *is* one event; the decision is documented there as needing to
be promoted or reverted, and this family is a second family that wants it.

### 8. A key the backend cannot produce is accepted silently

```console
$ omn io.bytes --system -m poll@100ms -d 300ms -e timeline -k task --format json
  # series keys: dev:253+0, dev:259+0 -- and no warning anywhere
```

`io.bytes` declares `keys: [blk, task, cgroup, dev]` because the chain as a
whole carries them; `fs:proc` attaches `dev` and nothing else. Asking for
`task` is accepted and produces events that carry no task, which for phase two
is the expensive kind of silence: a session written today to be joined
tomorrow produces events that cannot be joined to anything, and re-running it
is the same work twice. Compare `io.inflight -k task`, which *is* refused —
`io.inflight` does not carry the `task` key, so it cannot be joined on it —
because there the catalog does not declare it either. The check is against the
entry, never against the backend that was selected.

Stream B of the fifth wave found this cannot be fixed in `catalog/*.yaml` at
all: the per-backend `keys:` the brief says to use does not exist. `Backend`
carries `targets` and `filter_fields` in exactly that shape and no `keys`, so
`#[serde(deny_unknown_fields)]` refuses the data. The field, the two `Entry`
accessors, the loader check and the resolver's refusal are written out under
"Needs from another stream" in `docs/open-findings.md`; the entry meanwhile
carries a caveat naming which backend attaches which key.

This is `docs/open-findings.md` §1 ("the catalog declares per entry what
actually varies per backend") in its sharpest form: for modes it costs an exit
code, for filter fields it is caught and reported well — `--filter "comm == ..."`
names the backend that would carry the field — and for keys it costs nothing at
all until a join comes up empty.

### 9. The loss headline names three of the five things that are counted — **fixed**

Fixed in `src/encode.rs` by the fifth wave, by stream C rather than by the
stream this was given to: `Losses` is stream A's type and the headline is in
`src/encode.rs`, which stream C owns and stream F has no share of. The
reproduction below is the run as it was; what it prints now is at the end.

```console
$ # a /proc/diskstats holding one device this machine cannot classify
io.bytes  via fs:proc  (io.bytes)
  0 events over 300.35ms, stopped because the requested duration elapsed
  LOSS: 0 dropped, 0 failed reads, 0 late ticks
    4x `sda` has no entry under /sys/class/block, so whether its counters are
       its own could not be checked; it is left out of the total
```

`Losses` counts five things — dropped, failed reads, late ticks, undecoded and
throttled — and `src/encode.rs` printed the first three. A run whose only loss
is `undecoded`, which is this family's ordinary case, announced `LOSS` and then
three zeros, and the reader had to infer the real count from how many reason
lines followed. The structured formats carry all five, so this was the text
renderer alone; it is also the format in which someone is looking at the screen
deciding whether to trust the numbers. `undecoded` is not a small category
here: it is how `/proc/diskstats` says a line was left out of the total.

Three renderers printed the three, not one: the text output, the comment block
of a `pprof` profile, and the process label of a `chrome` trace.

**What it prints now.** One `loss_headline` serves all three, so the next kind
added to `Losses` is one edit rather than three:

```console
  LOSS: 0 dropped, 0 failed reads, 0 late ticks, 4 undecoded, 0 throttled
    4x `sda` has no entry under /sys/class/block, so whether its counters are
       its own could not be checked; it is left out of the total
```

The zeros are kept rather than elided. The line is only printed when something
was lost, and which of the five is zero is the part that says where to look: a
run losing only `undecoded` is a parsing gap and one losing only `throttled` is
a kernel that declined to deliver. Printing the non-zero ones alone would make
the vocabulary itself depend on the run.

`a_line_left_out_of_the_total_reaches_the_reader_at_a_terminal` asserts that all
five kinds are named and that `undecoded` carries a non-zero count. It fails
against the old headline with the defect quoted back:

```
`undecoded` is named with a count in `  LOSS: 0 dropped, 0 failed reads, 0 late ticks`
```

---

## Two flaky tests seen from here, neither in these files

Every test in this family passes on every run made here. Over a dozen
`cargo test --workspace` runs on a machine shared with other builds, two tests
outside it failed intermittently — about one run in six between them:

- `tests/budget.rs::the_extra_cpu_a_faster_poll_costs_is_the_extra_cpu_the_shell_observes`
  — *"the shell saw 0ns more cpu in the faster run and the tool reported
  41531000ns more"*.
- `tests/family_sched.rs::tasks_sharing_one_cpu_wait_the_share_the_scheduler_owes_them`.

Both compare two cpu-time readings taken at different instants, which is the
species `docs/open-findings.md` §11 already describes: a test that passes alone
and fails under load is not noise, it is two windows being pulled apart. The
second still fails with this family's four heavy scenarios skipped, so it is
not their doing; the first did not recur in five such runs, which says nothing
either way. Neither is in files this stream owns, and they are recorded here
rather than fixed for the same reason a defect is: the scenario found them, and
working around them would have measured nothing.

---

## Running these

```console
make                                  # the capabilities, and the workspace
cargo test --test scenario_io         # the scenarios, about four seconds
make check                            # everything, the way CI would
```

Four of the fifteen scenarios run for about two seconds each: they measure a
window that has to contain a workload, and a window shorter than the tool's own
startup plus 64 MiB of real I/O would be measuring the setup. The other eleven
are refusals, sweeps and substituted counters, and finish in milliseconds; the
sweeps run their dozen invocations concurrently, since none of them looks at a
number. The whole file takes four seconds, of which two are wall-clock windows
that nothing can shorten.

Where this machine cannot serve a scenario, it stops and names what is missing,
never quietly. There are three such limits. The scenarios that substitute
`/proc/diskstats` or hide `/proc/pressure` need unprivileged user namespaces —
`/proc/sys/user/max_user_namespaces` and `unshare`. The four that push real I/O
need a build directory on a real block device, mounted without compression;
tmpfs reaches no device at all, and compression turns a known number of bytes
written into an unknown number stored. And the pressure comparison needs a
machine that is not already stalled on I/O. None of them is a reason to stop
asserting the rest, and every one stays in `examples/` and here, so that a
machine which can serve it runs it.

---

## Found by the coordinator, after the merge

### The kernel's own `full` can exceed its own `some`, by a microsecond

```
stalled_ns       39670000     # some
full_stalled_ns  39671000     # full, one microsecond larger
```

One read of one file, under a saturated machine. `full` is a subset of `some` by
definition — every task stalled is at least one task stalled — but the kernel
keeps the two in separate accumulators and writes `/proc/pressure/io` in
microseconds, so they can disagree by one quantum of that.

Nothing in the tool is wrong here: it reported what the kernel said, which is
what it is for. The scenario now asserts the invariant with the resolution of
the thing that reports it, and a violation larger than a microsecond is the
tool's rather than the kernel's.
