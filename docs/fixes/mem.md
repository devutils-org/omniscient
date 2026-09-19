# Stream E — `mem`, `proc` and `power` readings

Eight items were handed to this stream: six places where a collector reported a
number that was not what it said, and two that turned out to belong elsewhere.
Six are fixed, each with the scenario that found it now asserting the new
behaviour and failing against the build that had the defect. One is left, and it
is named below with the one-line change it wants from the stream that owns the
file. One diagnosis was partly wrong, and saying so is the rest of this
document.

Every number below was read off this machine: an AMD Ryzen 9 3900X, kernel
7.2.6, transparent huge pages `always`, `perf_event_paranoid` at -1, and
`CAP_BPF` and `CAP_PERFMON` on the binary.

---

## Fixed

### mem D1 — `fs:proc:statm` invented a zero for a process that had gone

`src/collect/mem/footprint.rs`. A zombie keeps its `/proc/<pid>/statm` and that
file reads `0 0 0 0 0 0 0`. Nothing about the read fails, so the parser took the
second field, multiplied it by the page size and published 0 B as a resident
set — the one answer a footprint must never invent. The same zombie's
`smaps_rollup` answers `ESRCH`, so the two backends of one chain ended their
runs a reading apart and the self-benchmark reported them 6.25 % apart on a
value that was not a measurement.

The distinction could not be made on the `open`, which succeeds, so it is made
on the content. The first field of `statm` is the total size of the address
space, and a process that has one always has a non-zero total: an all-zero
`statm` is a target whose address space is gone, and it is now
`SampleError::TargetGone` with that reason. No history is needed, which matters
— the alternative, "zero where there was something a tick ago", would have been
wrong for the first reading of any run.

Measured on `examples/mem-backend-agreement.yml`: the `statm` series now ends
where the rollup's ends, its minimum is a real resident size rather than zero,
and the two last readings agree to within 1 %.

Asserted by `the_two_roads_to_a_footprint_arrive_at_the_same_number` in
`tests/scenario_mem.rs`, which now checks that no reading of a live process is
zero bytes and that both roads publish the same number of points.

### mem D2 — the shim stopped counting its drops as soon as it was dropping

`shim/src/lib.rs`, and it needed two changes rather than the one the diagnosis
named. See "mem D2 hid a second loss path behind the first" below for the
second.

The first is the one that was filed: when a backlog of counted drops was waiting
and the pipe was still full, `report` restored the backlog and returned through
a path that never counted *the record this call was for*. So the first record of
an overflow burst incremented the counter to one and every record behind it was
discarded in silence. Seven counted drops for thirty-one thousand lost records
is seven bursts. It now puts `dropped + 1` back, because a pipe too full for the
backlog is too full for the record behind it as well.

Measured on `examples/mem-allocation-storm.yml`, which makes exactly 200 000
allocations of 4001 bytes and frees each one: the records the run accounts for —
what the filter kept, what it dropped, and what the shim reports lost — is now
**406 018 on every run**, while the drop count itself ranges from 62 178 to
144 753 depending on how the pipe happened to fill. Before, the same total moved
between 339 000 and 406 018, and the difference was records that were neither
delivered nor counted.

Asserted by `the_shim_counts_every_record_it_could_not_carry`, rewritten around
what the workload actually guarantees — see "Something else" below for why the
original arithmetic could not close.

### mem D6 — the two `mem.thp` backends disagreed about resident huge pages

`src/collect/mem/thp.rs`. `fs:sys` read `nr_anon` out of the per-size statistics
and `fs:proc` read `nr_anon_transparent_hugepages` out of `/proc/vmstat`, and
the two were published under one attribute name, `resident_huge_pages`, 8 %
apart and systematically so. They are two different questions: `nr_anon` counts
every anonymous huge page of that size, including the ones that have had
subpages unmapped, while the vmstat counter — the one `/proc/meminfo` publishes
as `AnonHugePages` — counts only those still wholly mapped. The difference is
exactly `nr_anon_partially_mapped`, which the same sysfs directory publishes and
nothing read: 443 − 32 = 411 against vmstat's 411 on this machine while this was
written.

Of the three ways out the scenario listed, this takes the first and the second.
`fs:sys` now subtracts, so both backends answer the same question under the same
name; and what only `fs:sys` can see travels beside it as
`partially_mapped_huge_pages` rather than inside it. A kernel that publishes
`nr_anon` without `nr_anon_partially_mapped` cannot say how many are whole, so
there `resident_huge_pages` is absent rather than approximated — the same rule
the rest of this collector already followed for counters a kernel does not
publish.

`resident_huge_bytes` is still `resident × hpage_bytes`, which is what the
kernel itself reports as `AnonHugePages`.

Asserted by `both_huge_page_backends_read_the_same_promotion_counter`. That test
had to change shape as well as content: a resident count is a **level**, and the
session it was reading spreads its reads across half a second, over which this
machine promotes and splits huge pages the whole time. The comparison now runs
in a session of its own in which every request is a `snapshot`, so the two reads
are one round apart, and it is cross-checked against `/proc/meminfo` read either
side of the run.

A round apart is still not an instant on a loaded machine — the reading thread
can be descheduled between the two reads, and under the full suite the worst of
five runs was nineteen pages apart with nothing wrong. So the session is run
five times and the *closest* pair is asserted on. The drift is noise either way;
the defect was a bias, the same 8 % in the same direction on every run, and a
biased pair can never produce a close one.

### proc 1 — the first reading of a launched target described this tool

`src/collect/proc/lifecycle.rs`, new, and `src/collect/proc/status.rs`. The
runner releases the child from its gate and reads immediately; until the child
reaches its `execve` the pid is right and everything the pid holds belongs to
`omn`. In `poll` mode that was the first of several points and the series
recovered; in `snapshot` mode it was the only reading there was.

A pre-exec child is identifiable exactly: it is a child of this process *and* it
is running this same binary. The first read now waits for one of those two to
stop being true, polling every 200 µs for at most 100 ms, and reads then. The
deadline exists for the one case the test cannot distinguish — a run whose
target is `omn` itself — where waiting the whole deadline and reading late gives
the right answer anyway. A deadline that is reached is noted as a loss rather
than passed off as a measurement.

Measured on `examples/proc-one-reading.yml`: the single read said
`open=3 limit=524288 headroom=524285 comm=omn` and now says `comm=python3`.
`examples/proc-handles.yml`, which carries no filter, used to open its timeline
with this tool's own three descriptors and now opens with the program's.

Asserted by `defect_1_the_first_reading_of_a_launched_target_is_of_the_program`,
rewritten. **The assertion it used to make cannot be made** — see "Something
else".

### proc 2 — the last reading reported a zombie as holding nothing

Same two files. A process that has exited and not been reaped keeps a readable
`/proc/<pid>`: `fd` is an empty directory that reads successfully, and `status`
still carries a `Threads:` line saying one. Neither is the `ESRCH` nor the
`ENOENT` that `classify()` treats as "the target left", so the run took one more
sample and recorded zero open descriptors with the whole limit as headroom, no
loss, and a clean bill of health, for a program that never closed a handle in
its life. `min` and `last` of every polled launch were that zero and only `max`
was usable.

The collector now reads the process state out of `/proc/<pid>/stat` — from the
last `)` rather than by counting fields, because a program may call itself
`a b) c` — and reports nothing for a target in `Z` or `X`.

It reports nothing for one round rather than ending the run there, and that is
deliberate. The runner asks its own stop condition after each round, and for a
launched program that condition has the better reason: it knows the exit status,
so the run still says `stopped because the launched program exited with exit
status 0` rather than losing it to the collector's own wording. A `--pid` target
that nothing else is watching reaches the round after and is ended by the
collector.

Measured on `examples/proc-exit.yml`: `summary` was
`min 0, mean 12.6, max 20, last 0` and is now `min 4, mean 16, max 20, last 20`.

Asserted by
`defect_2_the_last_reading_of_a_launched_target_is_one_the_program_was_alive_for`,
which checks the descriptor series by value and the thread series by point
count — a zombie's `Threads:` line says one, which is indistinguishable by value
from the single-threaded program that was there a moment earlier, but the round
that read it added a point to both series and the round that no longer happens
removes it from both.

### power 1 — `power.energy -m count` was a series plus a zero

`src/collect/power/energy_pmu.rs`. Two causes that compounded, both now gone.

`EnergyPmu::sample` emitted a per-interval delta whether or not the mode was
`count`, so a counted three-second window came back as a series at the runner's
default poll interval. `EnergyPmu::stop` then took a further closing read that
covered microseconds and landed in that series as a zero. A `summary` of it said
the smallest interval was zero, the last was zero, and the mean was the window's
energy divided by one event more than there were intervals — against a `rate` of
the same window, which was right.

`EnergySysfs` already had the intended shape: nothing during the run, and one
whole-window delta from a baseline taken in `start`. So **the same request meant
two different things through the two backends of one chain**, which is exactly
what the backend comparison exists to find. `EnergyPmu` now follows the same
convention: the baseline is taken in `start` once the counters are enabled,
`sample` returns nothing in `count` mode, and `stop` emits one event per domain
for the whole window.

Every energy event from this backend now also carries `interval_ns`, the window
its difference covers, which the sysfs backend has always carried. A reader can
divide by it without assuming the tick was the one that was asked for.

Measured: `-m count -e summary -d 3s` reported `n=4 min 0.000 uJ mean 51.11 J
last 0.000 uJ` and now reports `n=1 min 270.80 J mean 270.80 J max 270.80 J
last 270.80 J` for a busier three seconds, with `rate` agreeing on the same run.

Asserted by `a_counted_energy_window_is_one_total_and_not_a_series_with_a_zero`
in `tests/scenario_power.rs`, which checks the shape, checks that nothing in it
is the closing read's zero, and compares the counted window against a polled
series over **the same** window — one monotone hardware counter differenced once
and differenced ten times has to cover the same joules. The two requests share
one session deliberately: two consecutive two-second windows on a machine
running the rest of this suite are two different machines, measured at 145 W and
102 W with nothing wrong at all. The `fs:sys` half of the comparison skips by
naming root, because `/sys/class/powercap/.../energy_uj` is root-only on this
kernel.

---

## Something else

### proc 1 was two things, and only one of them is a defect

The scenario's test asserted that over five snapshot runs against a program
holding twenty descriptors, at least one answer must differ from twenty — and
its failure message said that a snapshot which always found twenty would mean
the defect was fixed. That cannot be made true, and it should not be.

A snapshot is one read taken as early as there is anything to read. After the
fix the read happens immediately after the program's `execve`, at which point
`python3` holds its three standard streams and has not run its first statement.
Twenty descriptors is what the program holds twenty milliseconds later, once it
has opened its files, and a measurement tool cannot wait for that: it does not
know what the program is going to do, and any delay it chose would be a number
invented by the tool and reported as the program's.

So the defect is the first half only — that the reading was of the *wrong
subject*, and which subject depended on scheduling. That is fixed and is what
the scenario now asserts: five runs, every one naming the launched program, and
the five counts agreeing with each other to within the one descriptor `python3
-c` opens and closes while it starts. What was unspecified is now specified.

`examples/proc-one-reading.yml` already tells a reader to use `--pid` when they
want to know what a running process holds *now*, and that advice is still right.

### mem D2 hid a second loss path behind the first

With the counted-record fix in place the arithmetic still did not close, and it
closed differently on every run: the accounted total moved between 399 642 and
406 018 for a program that causes the same records every time.

The backlog rides out on the next record that gets through, and at the end of a
run there is no next record. Whatever was dropped in the last burst was counted
in the shim's own counter and never surfaced — up to six thousand records on
this workload. That is the same invariant broken one step further along: counted
is not the same as reported.

The shim now flushes the backlog from a `.fini_array` destructor, retrying a
full pipe up to two hundred times a millisecond apart. It is the one place this
shim waits for the pipe and it is allowed to: the program's last instruction has
already run, so what is being delayed is the teardown of a process that has
finished rather than the work being measured. The retry is bounded so that a
reader which has already gone cannot turn the end of a program into a hang.

With both halves in, the accounted total is 406 018 on every run of that
workload, on an idle machine and on one running four copies of it at once.

### The storm's arithmetic could not close in the form it was written

The scenario asked for `seen + dropped == 200_000`, and that could not be true
even with perfect accounting, because the two numbers are not commensurable.
`seen` is what survived the session's `bytes == 4001` filter; a drop is a record
that never reached the reader at all, so it can be the `free` of one of those
allocations or the interpreter's own traffic. Adding the filter's own count back
makes them comparable.

The test now closes over records rather than over the filtered series, and takes
its floor from what the workload guarantees: `free(malloc(4001))` two hundred
thousand times is four hundred thousand records, whatever else `python3` does
around them. Above that it asserts the stronger and more useful thing — the
same deterministic program, run twice, loses wildly different amounts to a pipe
that fills differently every time and **accounts for the same total**. A total
that moves with how much was lost is a loss that is not being counted.

---

## Left

### power 5 — a unit test that fails under load

`collect::tests::what_a_run_reports_spending_is_the_cpu_it_actually_burned` is
in `src/collect/mod.rs`, which belongs to stream A. It is reproduced here: eight
runs of `cargo test --lib` on this machine while the rest of this wave was
building produced one failure, with the message the scenario document quotes.

The cause is precise. The forked child drives the collector until a **wall**
deadline 200 ms out, accumulates the **cpu** it actually burned, and refuses to
conclude anything unless that came to at least 150 ms. On a machine with cores
to spare the two are the same number; on a busy one the child is descheduled and
the floor is not reached.

The request is one line and is written out in `docs/open-findings.md` under
"Needs from another stream": drive until the collector has burned the cpu rather
than until the wall clock says so. The test then measures exactly what it was
written to measure — that the spending meter agrees with the collector's own
account of its cpu — and cannot be decided by what else the machine is doing.

### The counted window still rounds up to the runner's tick

`power.energy -m count -d 600ms` observes 1.0002 s, and did before this change
as well. It is not the collector: in `count` mode the collector now asks for
nothing between `start` and `stop`. The runner checks its deadline once per
round, and with no collector declaring a pace it takes the interval from
`resolved[0].request.mode`, which for `count` falls through to
`Mode::DEFAULT_POLL_INTERVAL`.

This stream deliberately did **not** work around it by declaring a short
`tick()` for a counted energy collector. The run's interval is the minimum over
its collectors, so a counted `power.energy` request declaring 10 ms would drag
every other request in the session down to 10 ms — which is the runner defect
`power` 4 and `proc` 3, made worse rather than better. It belongs to stream A
and is recorded as such.

### Two example sessions now describe a defect that is gone

`examples/proc-one-reading.yml` and `examples/proc-exit.yml` carry header
comments explaining, at length, the defect each was written to show. Both are
now fixed, and a defect that is gone and still documented as present is the same
kind of lie this project is about. `examples/` belongs to no stream in this
wave, so the replacement text is written out in `docs/open-findings.md` for the
coordinator rather than applied here.

---

## Declared: one edit outside this stream's paths

`shim/src/lib.rs` is not under `src/collect/{mem,proc,power}/`, and mem D2 lives
nowhere else. `BRIEF.md` assigns the defect to this stream, no other stream owns
the shim, and `docs/scenarios/mem.md` says the fix "should go first" because it
masks the loss paths underneath it. Both changes to it are in `report()` and its
neighbourhood and are described above.

---

## What was exercised, and what could not be

Everything claimed here was run on this machine. `make check` is clean.

Two things this machine could not serve, both named where they matter:

- **The `fs:sys` half of the `power.energy` backend comparison.**
  `/sys/class/powercap/intel-rapl:0/energy_uj` is root-only on this kernel, so
  the counted shape of that backend is checked by reading its code and by the
  unit tests over a scratch tree, not by a run. The scenario skips that half
  loudly and names root.
- **A kernel without `nr_anon_partially_mapped`.** The absent-rather-than-
  approximated path in `mem.thp` is exercised by a unit test over a scratch
  tree, because this kernel publishes the counter.

One thing to know before reading a red suite from this wave:
`tests/scenario_net.rs::the_count_leg_of_the_sweep_finds_the_traffic_the_scenario_pushed`
failed once here while several suites ran at once, measuring the machine's
loopback traffic rather than its own. It is stream F's family and the shape
`STATE.md` already warns about.
