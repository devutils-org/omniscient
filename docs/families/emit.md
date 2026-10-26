# `emit` — the distribution that says it is short

Not a family: the aggregation and presentation layer, `src/emit.rs` and what
renders what it produces. Stream F of the sixth wave, and the one stream of
that wave that adds no source. Its whole subject is finding 37 of
`docs/open-findings.md` — a `hist` or a `summary` built from a run that lost
events renders exactly like one built from a complete run, and the events it is
missing are the slow ones.

## What was built

A `Shortfall`: what a lossy run's distribution is missing, which way that
biases it, and `unknown` where the number is genuinely unknown.

It is carried on `SeriesHist` and `SeriesSummary` rather than on `Results`,
which is the whole point of the finding. The loss counters were always there
and always correct; a reader taking a p99 off a result file had them nowhere
near the number. On the series they arrive beside it in every format at once.

```console
$ omn mem.heap.alloc -m trace -e hist -- python3 -c '<200 000 allocations>'
  357556 events over 558.99ms, stopped because the launched program exited
  LOSS: 48484 dropped, 0 failed reads, 14 late ticks, 0 undecoded, 0 throttled
    6x the shim could not keep up: allocations happened faster than the pipe drained

  hist (log2)
    task:505187  n>=357556  min 0 B  mean >=48.0 KiB  max >=256.0 KiB
      ...
      [64 KiB, 128 KiB)                175818  |############################### |
      ...
    >= short by at least 48484 of 406040 events (11.9 % unaccounted for): the ring
       had nowhere to put them. What stalls a machine is what stops the measuring,
       so the events it is missing are the slow ones, and every count, mean and
       maximum marked `>=` is a lower bound rather than an estimate
```

That program made **two hundred thousand** allocations of one size, and the
bucket that holds that size reports **175 818**. The old output said `n=357556`
and nothing else; the reader had no way to know the run was a tenth short, and
a triage layer reading the p99 would have read it as a measurement.

## Three decisions, each of which could have gone the other way

### The mark goes on the numbers a loss pulls, and on no others

`n`, `mean` and `max` are lower bounds. `min` is not, and `last` is not. The
events a lossy run is missing are the *slow* ones, so the smallest value
measured is the one statistic the loss does not move, and marking it would be
claiming a bias that does not exist. A mark that means "something about this
run is uncertain" is a mark nobody can act on; `>=` means the true number is
above the one printed, and that is actionable.

The bucket counts are lower bounds too, and are not marked one by one: a log2
histogram is up to sixty-five rows and saying it sixty-five times is how a mark
becomes wallpaper. The head line carries it and the sentence below says which
end of the distribution the missing values were at.

### A throttle has no numerator, so no fraction is printed

`Losses::dropped` is a count of records and yields a fraction.
`Losses::throttled` is a count of *windows in which the kernel declined to
deliver*, and how many events happened inside one is recorded nowhere.
`docs/DECISIONS.md` already chose that answer for the loss line; this says the same
thing rather than inventing an estimate:

```
>= short by an unknown number of events: the kernel throttled this event 243
   times and how many events happened inside those windows is not recorded
   anywhere. ... every count, mean and maximum marked `>=` is a lower bound
```

A run that did both reports the fraction it knows as a **floor** and the rest
as unknown — `short by at least 10064 of 59918 events (16.8 % unaccounted
for), and by an unknown number more` — rather than folding one into the other
and producing a number that is neither.

### Two of the five losses do not mark a distribution, each for its own reason

`loss_headline` still prints all five, unchanged. What is new marks on two:

| Loss | Marks a distribution | Why |
|---|---|---|
| `dropped` | **yes** | records the ring had nowhere to put, which is the busy period by definition |
| `throttled` | **yes**, as unknown | the kernel declining to deliver, which happens when the rate is highest |
| `late_ticks` | no | `poll`'s loss, and `poll` cannot produce `hist` at all |
| `failed_reads` | no | the same |
| `undecoded` | no | a record shape this build cannot parse; nothing suggests the shapes it cannot parse are the slow ones |

The `poll` half was closed before this stream existed and for a better reason.
`docs/CATALOG.md` §12 refuses `poll` with `hist` because *a histogram of a polled
gauge is a histogram of when the poller happened to look*, which is the
coordinated-omission argument written down before anyone here used the term.
`src/validate.rs`'s `the_mode_emit_matrix_is_enforced` is the test; this stream
did not duplicate it.

## The line between a weight and a loss

`emit::WEIGHT_ATTR` says a kernel-built bucket stands for N measurements and is
**complete**. A run that dropped N records is **not**. Conflating the two would
have been the one way this mark could make the output worse, and stream C is
about to become `WEIGHT_ATTR`'s heaviest user.

They meet in one place: a run whose rows are weighted *and* lossy. There the
fraction that is knowable is a fraction of **rows**, because what the dropped
rows would have weighed is a number nobody upstream ever saw. So the note says
rows, `Shortfall::per_record` records which kind of thing is being counted, and
nothing claims a fraction of the distribution:

```
short by at least 1 of 4 rows (25.0 % unaccounted for) ... Each row stands for
more than one measurement, so the fraction is of the rows this backend handed
up and not of the distribution: what the dropped rows weighed is not knowable
up here
```

`per_record` is true when any event carries `WEIGHT_ATTR` above one or
`TOTAL_OF_ATTR` at all — the same two attributes findings 17 and 18 are about,
and the same distinction `pre_aggregated` already draws for the values
themselves.

A weighted run that lost nothing gains nothing, which is checked: weighing is
not losing.

## What is checked, and against what

| Check | The independently known thing |
|---|---|
| `tests/aggregation.rs`, the drop path | two hundred thousand allocations the test made, against a bucket that holds fewer, against a drop count the **shim** reported |
| the conservation check in the same test | every allocation either reached a bucket or was counted as a loss, so bucket + dropped cannot come to less than the storm |
| `tests/aggregation.rs`, the throttle path | the kernel's own `PERF_RECORD_THROTTLE` count, against a note that must not contain a percentage |
| `tests/aggregation.rs`, the negative | the same workload two hundred allocations wide, which loses nothing and must gain nothing |
| `tests/encoders.rs` | every format whose `supports_emit` says it can carry a distribution, read off the matrix rather than listed |
| `src/emit.rs` unit tests | fractions the test chose, and the four wordings |

The losses in `tests/encoders.rs` are **substituted** rather than provoked,
which is the `io` scenarios' pattern with a `/proc/diskstats` of their own
writing: a ring that drops is a property of a machine under pressure and not of
a workload, and what is under test there is the encoder. The losses in
`tests/aggregation.rs` are real.

### The negative is half the work

A clean run gains nothing: not a field, not a mark, not a character. Checked in
every format, and by absence of the whole vocabulary rather than by a mark that
says zero. A mark that appears on every run is one readers learn to skip, and
then the run that needed it is the one nobody reads it on.

### Where a property could not be created here, and what the test says instead

Both loss paths were provoked for real on the development machine, so neither
skipped here. Both can skip elsewhere, loudly and with the numbers:

- **No drop.** A machine that drains the shim's pipe as fast as python can fill
  it has nothing to mark. The skip prints the allocations asked for, the events
  that arrived, the observed window and what reached the bucket — and still
  asserts that an unlossy run carries no shortfall, so the test is not silent.
- **No throttle.** Whether a hundred thousand samples a second is throttled is
  set by `kernel.perf_event_max_sample_rate` (100 000 here) and
  `kernel.perf_cpu_time_max_percent` (25 here), which are the machine's
  settings. The skip prints both, with the samples that arrived and the window.
- **No `cpu.profile` at all.** A machine with `perf_event_paranoid` above what
  the binary carries cannot sample. The skip prints the level — but first
  asserts that the refusal is not `cannot emit` or `no collector`, because a
  request that cannot mean anything is not a machine that cannot serve it.

## What was deliberately not changed

- **`poll` with `hist`.** Refused already, for the better reason. Untouched.
- **The loss counters and `loss_headline`.** Counted and surfaced already; the
  invariant held. This is the step after it, not a replacement for it.
- **`rate`, `topk` and `stack`.** Understated by the same losses, and not
  marked. The finding, `docs/dev/BRIEF.md` and `docs/dev/STATE.md` all scope themselves to
  distributions, and widening the scope inside this stream would be deciding
  something three documents left alone. The argument for widening it is real
  and is written up under "Needs from another stream" in
  `docs/open-findings.md`; `Output::Stack` already carries `unattributed` and
  `Output::TopK` already carries `Rest`, which are each that emit's own version
  of this, so the shape is not obvious and belongs to whoever takes it.

## Needs from, and given to, another stream

`src/render.rs` renders requests, listings and refusals. It does **not** render
results — `src/encode.rs` does, and `hist_text` lives there. Finding 37 says
the defect "belongs to whoever owns `src/emit.rs` and `src/render.rs`", which
is a mistake about which file is the rendering layer rather than about what had
to change. So the text rendering of the mark is nine lines in `src/encode.rs`,
which no stream of this wave owns; `src/render.rs` is untouched. It is written
up for the coordinator under "Needs from another stream" in
`docs/open-findings.md`.
