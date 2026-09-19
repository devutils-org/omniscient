# Stream F — `net`, `io`, `gpu` and `runtime` readings

Eight defects, from four families that share no mechanism. Six are fixed, with
the scenario that found each one now asserting the new behaviour; two are left,
both because the change they need is in a layer this stream does not own, and
both are written up under "Needs from another stream" in
`docs/open-findings.md` with the signature wanted.

This machine could exercise everything. It has an NVIDIA RTX 2080 Ti, so the
`gpu` scenarios measured rather than skipped; `unshare` and `iproute2` are
present, so the `net` and `io` scenarios that substitute a file or build a veth
pair ran for real. Nothing below is a fix that was reasoned about and not run.

---

## Fixed

### net 3 — `snapshot` on a counter source can only ever answer zero

`net.iface` and `net.backlog` declare `snapshot` and read counters. A snapshot's
two reads were its one read and the closing read that followed it, about a
hundred microseconds apart, so what came back was an increment over a hundred
microseconds — zero on anything short of a saturated machine, and zero for an
accept queue that was demonstrably overflowing.

A single read of a counter cannot be an increment. What it can be is the counter,
and that is what both sources now report in `snapshot` mode, following the shape
`mem.swap` already used for exactly this: the value attribute is renamed so that
nobody can read one number as the other, every counter carries a `_since_boot`
suffix, and a `basis` attribute says in words what the event is.

```console
$ omn net.backlog --system --mode snapshot --emit raw
  raw
    88672974229654  net.backlog    drops_since_boot=33229 overflows_since_boot=33229
    syncookies_sent_since_boot=0 syncookies_failed_since_boot=0
    basis=the counters as they stand: a snapshot observes no window of its own
```

`stop()` takes no closing read in this mode, because there is no window to
close; taking one would have replaced the answer with the microsecond increment
that was the defect.

The ground truth is unchanged and now cuts both ways.
`a_snapshot_of_the_accept_queue_reports_the_queue_the_kernel_holds` still
hammers a one-deep listener and still measures the drops with `count`, and the
snapshot taken afterwards must both *contain* that second's drops and be far
larger than any increment over its own microsecond window could be. Neither
bound is the tool's own arithmetic. Two unit tests check the same thing against
`/proc/net/dev` and `/proc/net/netstat` read independently.

### net 5 — every aggregating emit of `net.iface` answers about errors alone

Half fixed, and the half that is not is another stream's.

The collector's measured value is `errors`, deliberately: the catalog's own
caveat says a bad frame and a full queue are different failures and summing them
would hide which happened. The consequence was that a link which had just
dropped exactly a thousand frames reported `0 count/s` through `rate`,
`timeline` and `summary`, and said nothing about it.

Splitting the series by which counter it came from is the real fix and it is not
available here: a series is keyed on the first key of its events, and the key
model has no variant for "which counter", so every per-counter event of one
interface would collapse back into one series and be summed — which is the thing
the caveat forbids. That is written up as a need.

What *is* available is to stop the omission being silent. The collector now
accumulates what each interface lost to the counters the series does not carry,
and a run where that is non-zero says so in the one channel every emit and every
format carries:

```
  degraded: `veth0` also reported 1000 drops over this run; the measured value
  of `net.iface` is errors, so every emit but `raw` answers about errors alone
  and these are not in it
```

`every_frame_a_down_peer_refuses_is_counted_as_a_drop_and_none_as_an_error`
keeps its ground truth — a thousand datagrams handed to a link whose far end is
down — and now asserts both halves: the rate still reports errors alone, and the
run names the thousand frames it is leaving out of it.

### io 4 — a dropped interval, noted and not counted, in a run reporting `no loss`

A device that appears in `/proc/diskstats` part way through a run has no earlier
reading to be measured against, so its first interval is not collected. The
collector recorded the reason and incremented nothing, which left
`Losses::is_clean` true, printed `no loss`, and — because reasons are only
rendered under a non-zero count — never showed the reason either. One line: it
now increments `undecoded`, which is the category the sibling case one line
further down already used for the same shape of gap.

The scenario deliberately did not assert the count, so that fixing this would
not have to fight the test. It asserts it now, and it does so in a way that
discriminates: the three reasons that leave a reading out of the total are
summed from the reasons map and `undecoded` must be at least that. Confirmed by
reverting the one line and watching
`substituted_counters_are_converted_exactly_and_their_gaps_are_named` fail.

### gpu 1 — the one time error this family records reaches no output it can produce

Half fixed, on the same pattern as net 5.

`gpu.engine_busy` is the only source here whose samples do not happen when they
are timestamped: NVML averages occupancy over a window it chooses and does not
report, documented as up to a second wide, and every event carries
`ts_err_ns = 1_000_000_000`. `raw` is the only emit that serialises that field
and this source does not declare it, so a reader of a timeline saw four points
200 ms apart with nothing to say they might all be one driver sample.

Declaring `raw` is a catalog change and putting an error bar on a timeline point
is an emit change; neither is this stream's. What the collector can do, and now
does, is state the error bar once as a degradation of the run, which reaches
`text`, `json` and `chrome` whatever emit was asked for:

```
  degraded: vendor:nvml: occupancy is averaged by the driver over a window it
  chooses, documented as up to 1000 ms wide, so every timestamp in this series is
  good only to that much and two points closer together than it may be one sample
  read twice
```

`ndjson` is the one format that carries nothing of this, and that is a property
of the format — a stream of points with no run-level object — rather than of
this source. The scenario, renamed
`the_one_time_error_this_family_records_reaches_every_output_that_can_carry_it`,
walks every emit the source declares in every format that has somewhere to put
a note, and requires both the statement and its *size*: an uncertainty with no
number is not one.

### gpu 2 — a `count`-mode run over-reports its own window by up to a second

`gpu.engine_busy` is the only `gpu` source declaring `count`, and a `count`
names no cadence of its own. No collector in the family declared a tick, so the
run fell back to `Mode::DEFAULT_POLL_INTERVAL`, noticed a launched program's
exit only at the next tick, and reported a window that was the program's life
rounded up to the next second. `rate` divides by that window, and `rate` is
reachable on this source only through `count`.

The collector now takes the mode and, in `count`, splits two things that were
one: it declares a ten-millisecond tick so the *run* notices its target going,
and throttles its own reads to `Mode::DEFAULT_POLL_INTERVAL`, which is about
where the driver refreshes what it reads. Without the split, either the window
stays rounded up or the driver is asked a hundred times for one sample — and an
NVML read is milliseconds, not microseconds. The closing read happens in
`stop()`, which is where a `count` window is closed. The same tick, with the
same justification, is in `collect/cpu/counters.rs`; the comment there is where
the idiom was worked out.

```
before   sleep 0.3 : window 1.003 s, 2 events
         sleep 1.5 : window 2.003 s, 3 events
after    sleep 0.3 : window 0.303 s, 2 events
         sleep 1.5 : window 1.510 s, 3 events
```

The ground truth is the program: `sleep 0.3` lives 300 ms.
`a_count_mode_run_reports_the_window_of_the_program_it_watched` requires the
window to be that, and the run to have stopped because its program did.

### gpu 6 — a target that exited but was not reaped is followed to the end of the run

`gpu.mem` ended a run when its process went, and asked whether `/proc/<pid>`
existed. A zombie has one. A parent that has not reaped its child — a
supervisor, a test harness, anything using `posix_spawn` and deferring the
`wait` — therefore left the collector polling a process that no longer existed,
producing exactly the series of zeros the code's own comment says it is there to
prevent.

The state letter in `/proc/<pid>/stat` is what tells the two apart, and it is
now what is read: `Z` and `X` are both departed, and the run ends naming which.
The field is counted from the last `)` because a command name may contain spaces
and parentheses, which is the same reading `collect/mod.rs` does for cpu time.

```console
$ # a child that exits after 200 ms and is deliberately not reaped
$ omn gpu.mem --pid 485002 --mode poll@100ms --emit summary -d 800ms
  2 events over 200.27ms, stopped because process 485002 exited and has not been
  reaped, so its video memory is already gone
```

`a_target_that_exits_without_being_reaped_ends_the_run` asserts the run stops at
the child rather than at the duration, and that what it reported is the process
while it existed rather than a tail of zeros for one that did not. A unit test
beside it waits for a child of its own to reach `Z` and checks that the `/proc`
entry is still there while `departed` says it has gone — which is the whole
defect in two assertions.

---

## Left, and why

### io 7 — `io.bytes` measures the read/write split and no emit can show it

Not this stream's to fix, and the recorded diagnosis already says where it
belongs. `io.bytes` puts `read_bytes`, `write_bytes`, `sector_bytes` and
`discarded_bytes` on every event; the catalog declares
`emits: [rate, summary, timeline, topk]`, none of which carries an event's
attributes, and `raw`, which does, is not declared and is refused by name. The
same holds for `io.iops` and `io.merges`.

Every way out is a catalog or emit change: declare `raw` on the three entries,
or promote the `count` → `raw` pairing that the `cpu` thread added to the mode ×
emit matrix for this exact reason and that `docs/open-findings.md` §5 records as
needing to be promoted or reverted. The signature is under "Needs from another
stream"; it is stream B's line to write.

The one thing this stream considered and rejected was stating the split in a
note, the way net 5 and gpu 1 now state what they leave out. Those two notes
are honest because something genuinely went missing: a class of failure absent
from the series, a timestamp that is not an instant. An `io.bytes` run's split
is not missing, it is *detail* — and a note on every run that moved a byte is
noise in the channel this project keeps for degradations.

### io 9 — the loss headline names three of the five things that are counted

`Losses` counts five things — dropped, failed reads, late ticks, undecoded and
throttled — and the text renderer prints the first three. A run whose only loss
is `undecoded`, which is this family's ordinary case and how `/proc/diskstats`
says a line was left out of the total, announces `LOSS` and then three zeros,
leaving the reader to infer the real count from how many reason lines follow.

The renderer is `src/encode.rs`, which is stream C's. The structured formats
already carry all five, so this is the text renderer alone — and the text
renderer is the format in which someone is looking at a screen deciding whether
to trust the numbers. The exact line wanted is under "Needs from another
stream".

Fixing io 4 made this sharper rather than softer: the ordinary `io` loss is now
*more* likely to be the category the headline does not name.

---

## Nothing was found to be something else

Every one of the eight was what its entry said it was. The two left are left
because of where the change belongs, not because the diagnosis was wrong.

One judgement is worth recording, because it was reversed. The first attempt at
gpu 2 refused `count` on the NVML backend outright, on the grounds that the
entry declares `count` for the kernel PMUs — where engine occupancy is a
busy-nanosecond counter — and that NVML has no counter to difference, so serving
`count` from it means integrating a sampled ratio, which is the arithmetic this
same file already refuses for energy. That reasoning still looks right, and it
was withdrawn for two reasons that have nothing to do with it. It would have
made `count` unreachable and so quietly closed **gpu 3**, which is stream B's
defect and a different decision. And it would have added a ninth instance of the
pattern **io 1** and **net 7** record — a mode that resolves and then refuses at
attach — which the family's own sweep,
`every_mode_and_emit_a_collected_source_declares_is_served_or_refused_by_name`,
exists to keep out. A refusal belongs in the validator, which means the catalog
declaring modes per backend; that is `docs/open-findings.md` §1 and it is
written up again as a need.

---

## `runtime`

This stream owns `src/collect/runtime/**` and was given no defect in it. The two
`runtime` scenarios record are both in the shared layer: `topk` ranks series and
`runtime.jit` carries one key, so a ranking of *methods* needs either an emit
that groups on an attribute or `method` as a key (stream B and stream C, and the
brief routes it to both); and `=~` is `str::contains` while every language that
spells a match operator that way means a regular expression (stream C). Neither
is reachable from a collector. `tests/scenario_runtime.rs` is untouched.

---

## Two edits outside this stream's paths, declared

Both are comments that documented a defect this stream fixed, in files no stream
in this wave owns. A defect that is gone and still documented as present is the
same kind of lie the brief names, and leaving them would have left them nobody's.

- `examples/net-snapshot.yml`: the header said these numbers "are increments
  over a hundred microseconds, not the totals since boot, and on an idle machine
  every one of them is zero", and pointed at the defect entry. It now says what
  a snapshot reports, and names the attribute suffix and the `basis` attribute.
- `examples/gpu-device-state.yml`: the comment beside the `busy` request said
  "no emit this source declares can show it" of the one-second `ts_err_ns`. It
  now says the error bar is stated as a degradation and that what still has
  nowhere to go is the number on each point.

---

## Running these

```console
make                                  # the capabilities, and the workspace
cargo test --test scenario_net        # about four seconds
cargo test --test scenario_io         # about four seconds
cargo test --test scenario_gpu        # about twelve, and it needs the device
make check                            # everything, the way CI would
```

`make check` was run four times against this branch, two of them clean end to
end — 993 tests, clippy and fmt. None of the failures seen across the four is in
a file this stream owns, every one is the species `STATE.md` already names —
*a measurement of a shared machine is not repeatable inside a parallel test
suite* — and every one passes in isolation. The machine carried a load average
between six and ten throughout, because seven other worktrees were running their
own suites at the same time.

- `collect::tests::what_a_run_reports_spending_is_the_cpu_it_actually_burned`
  (`src/collect/mod.rs`, the shared layer) — *"the collector was asked to spin
  for 200 ms and did not, so this measured nothing"*. Passes three times out of
  three in isolation.
- `tasks_sharing_one_cpu_wait_the_share_the_scheduler_owes_them`
  (`tests/family_sched.rs`) — *"a task alone on a cpu waits for nothing, and
  this one waited 44 % of the window"*, which is word for word what
  `docs/scenarios/net.md` already records for this test under load, and
  `the_kernels_buckets_are_the_emit_layers_buckets` failing behind it on the cpu
  lock the first one poisoned. Both pass twice out of two in isolation.
- `the_process_figure_and_the_device_figure_move_by_the_same_step` and
  `a_resident_kernel_separates_from_an_idle_device_on_occupancy_and_on_power`
  (`tests/scenario_gpu.rs`) — one each, in two earlier full-suite runs, a
  different one each time. Both read numbers off the device; the suite already
  serialises on a lock for that reason and `docs/scenarios/gpu.md` records the
  sensitivity. Five runs each in isolation, all green.

Recorded rather than worked around, and recorded here rather than as a new
defect, because whoever merges eight branches will meet all three.
