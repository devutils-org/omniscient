# Stream C — emit, filter, render

What was fixed, what turned out to be something else, and what was left. The
five defects `docs/dev/BRIEF.md` gave this stream are sched 1, runtime 1, runtime 2,
net 6 and mem D4; a sixth, io 9, was assigned elsewhere and is in this stream's
files, so it is here too.

The short version: **two are fixed end to end, one is half fixed because its
other half is one line in a file stream D owns, and three are not defects in
these files at all.** Each of the three names the file it belongs in and the
exact change wanted, both here and under `## Needs from another stream` in
`docs/open-findings.md`.

| Defect | Where it actually was | State |
|---|---|---|
| io 9 — the loss headline names three of five | `src/encode.rs` | **fixed**, scenario asserts it |
| runtime 2 — `=~` is a substring test and nothing says so | `src/filter.rs` | **fixed**, scenario asserts it |
| sched 1 — `--emit hist` renders a distribution that is not one | `src/emit.rs` **and** `src/collect/sched/offcpu.rs` | mechanism landed; one line needed from stream D |
| runtime 1 — `topk` ranks processes, not methods | `src/request.rs`, then `src/emit.rs` | not fixed: the surface syntax is in a file nobody owns this wave |
| net 6 — `daddr ==` can never match | `src/collect/net/tcp.rs` (stream F) | not a filter defect |
| mem D4 — a refusal sentence in a slot meant for one word | `src/collect/mod.rs` and `src/error.rs` (stream A) | not an emit defect |

---

## Fixed

### io 9 — the loss headline named three of the five things that are counted

`Losses` counts `dropped`, `failed_reads`, `late_ticks`, `undecoded` and
`throttled`, and `is_clean()` checks all five. Three renderers printed the first
three: the text output, the comment block of a `pprof` profile, and the process
label of a `chrome` trace. A run whose only loss is `undecoded` — which is how
`/proc/diskstats` says a line was left out of a total, and `io`'s ordinary case
— announced `LOSS` and then three zeros, and left the reader to infer the real
count from how many reason lines followed.

```console
$ # a /proc/diskstats holding one device with no /sys/class/block entry
  LOSS: 0 dropped, 0 failed reads, 0 late ticks          # before
  LOSS: 0 dropped, 0 failed reads, 0 late ticks, 4 undecoded, 0 throttled
```

One `loss_headline` function now serves all three renderers, so the next kind
added to `Losses` is one edit rather than three. The zeros are kept rather than
elided: the line is only printed when something *was* lost, and which of the
five is zero is the part that says where to look.

`tests/scenario_io.rs::a_line_left_out_of_the_total_reaches_the_reader_at_a_terminal`
substitutes a `/proc/diskstats` holding one device this machine has no `/sys`
entry for — a chosen input, not an observed one — and now asserts that all five
kinds are named and that `undecoded` carries a non-zero count. Against the old
headline it fails with the defect printed verbatim:

```
`undecoded` is named with a count in `  LOSS: 0 dropped, 0 failed reads, 0 late ticks`
```

This is the invariant in `CLAUDE.md` — *losses are counted and surfaced to the
user, always* — and it was half true in the one format a person actually reads.
`docs/dev/BRIEF.md` gave io 9 to stream F with the note that "the second is stream A's
type and your headline"; the type is stream A's and the headline is in
`src/encode.rs`, which is this stream's. Taken here for that reason. Stream F
has no file to fix it in.

### runtime 2 — `=~` is a substring test, and now it says so

`=~` is `str::contains`. Perl, Ruby, awk and bash's `[[ =~ ]]` all spell a
regular-expression match this way, so a reader has every reason to write one —
and a regex written here parsed, validated, was reported as applied, and kept
nothing:

```console
$ omn runtime.jit --filter "method =~ 'omniscientSwept'" ...   # keeps 4 of 4
$ omn runtime.jit --filter "method =~ 'Swept[1-4]'" ...        # kept 0 of 4
```

`kept 0 of 4` is also what a correct filter over a quiet workload prints, which
is what made this the same silent-empty failure as the `kind`/`code_kind`
collision this family already had once.

`filter::literal_only` now refuses, at parse time, a `=~` pattern that could
only have been meant as a regular expression:

```console
$ omn runtime.jit --filter "method =~ 'Swept[1-4]'" ...
error: `=~` is a substring test, not a regular expression, so `Swept[1-4]` would only keep
values containing `[` literally. `[` is a character class in a regular expression and
nothing in a substring: write the text to look for, as in `method =~ 'text'`.

$ omn proc.fd --filter "comm =~ '^python$'" ...
error: `=~` is a substring test, not a regular expression, so `^python$` would only keep
values containing `^` literally. It has no anchors: write `comm == 'python'` for an exact
match, or `comm =~ 'python'` to find it anywhere in the value.
```

The refusal is the *only* place in a run that has any reason to mention what the
operator does, which is why this was taken over the cheaper fix of one line of
manual. It happens before anything is attached: the request cannot mean what it
says, which is a different answer from a machine that cannot serve it.

**The set is deliberately narrow** — a character class `[`, a group `(`, an
escape `\`, and the two anchors in the positions where they anchor. `.`, `+`,
`*` and `?` are excluded on purpose: `a.js:3:1`, `libc.so.6+0x27781` and
`operator*` are ordinary method and frame text, and `daddr =~ '127.0.0.1'` is a
working predicate this family's own scenarios use. Refusing a pattern that
matches something is worse than the defect. Every `=~` in `examples/` and
`tests/` was checked against the new rule before it landed; none is refused.

`tests/scenario_runtime.rs::defect_2_the_match_operator_is_a_substring_test_and_says_so`
asserts both halves — a literal substring still keeps 4 of 4 and a longer one
narrows to 1, and five regex-shaped patterns are refused with a message naming
what `=~` is. The ground truth is unchanged: the workload writes eight entries
with names no plausible constant could produce.

**Half of it is left.** `man/omn.1` still lists `=~` among the operators without
saying what it does, and no stream in this wave owns that file. The scenario
keeps an assertion that it is still silent, pointing at
`docs/open-findings.md`; the exact troff line wanted is there.

---

## Not what the brief thought

### sched 1 — the cause named in the brief is not the cause

`docs/dev/BRIEF.md` says the likely cause is "the one `emit::WEIGHT_ATTR` was added for,
in a source that does not yet carry it — check before assuming". Checked, and it
is not. Weighting makes the number worse, not better.

`sched.offcpu` keeps `(task, state, stack) -> {total_ns, blocks}` in a BPF map
and a drain hands up one event per window. Measured here on a smaller workload
than the one in `docs/scenarios/sched.md`:

```
raw  total_ns=102859037 blocks=2 window_ns=100061433 …
hist [67108864ns, 134217728ns)  1
```

The row's value is the **sum** of two blocks. `WEIGHT_ATTR` means *this value
occurred N times*, which is true of a `sched.runqlat` row — `(task, bucket) ->
count`, where every wait behind it had a value inside that bucket. It is false
here: weighting this row by 2 would report two blocks of 102.86 ms each, and
weighting the document's row by 400 would put four hundred blocks of about
twenty microseconds in the bucket of their sum, a range no single one of them
was in. The unit test
`emit::tests::a_row_that_is_a_total_over_its_measurements_says_what_it_is`
carries that arithmetic so the diagnosis is not re-made.

`docs/open-findings.md` §11's "From stream A (aggregation), for `sched.offcpu
--emit hist`" reached the same conclusion in an earlier wave and stated the real
fix: *"the fix is in the key, not in the emit"* — a log2 bucket in the
aggregation key, so the kernel counts `(task, state, bucket) -> blocks` and the
histogram falls out of the map, as `runqlat` already does. That file is
`src/collect/sched/offcpu.rs`, which stream D owns and is editing for sched 2
and sched 3.

**What landed here** is the half that is the emit layer's, and it is the half
the scenario document asks for — *"until then `hist` on this source should carry
a degradation of its own or be refused, because a reader has no way to tell the
two histograms apart"*:

`emit::TOTAL_OF_ATTR` — a collector saying *this value is the sum over N
measurements whose individual values do not exist up here*. `raw`, `rate`,
`timeline` and `topk` are unaffected by it, since a total is exactly what they
add up or plot. `hist` and `summary` are the two that claim to describe
individual measurements, and `aggregate` now puts a degradation beside them:

```
degraded: `hist` and `summary` describe the 9 rows this backend handed up and not the
19951 measurements behind them: the values are added together before they reach the emit
layer, so every bucket, mean and extreme here is of a total over a drain window rather
than of a measurement. `total_of` on each row says how many it stands for
```

Degradation rather than refusal, for three reasons: the scenario document lists
it first; `sched.runqlat` is the in-repo precedent and it degrades rather than
refuses; and a source whose catalog entry declares `hist` and whose run then
refuses it is the io-1 and sched-9 defect class this very wave is fixing.

It is **inert until a collector opts in**, so nothing in the tool changes today
and no scenario can assert it end to end. The one line stream D needs is in
`docs/open-findings.md`. That line was added to `offcpu.rs`
here, run against the workload of defect 1, and reverted — nothing in this
stream's diff touches that file — so the request is exercised rather than
guessed at, and the output it produces is quoted there. When it lands, the defect is visible, and when the
kernel-side bucket lands after it, the attribute goes away and `WEIGHT_ATTR`
takes over — which is the outcome the whole mechanism is shaped to reach.

### runtime 1 — `topk` cannot be fixed from inside the emit layer

The scenario states the two possible fixes: *"either `topk` needs to be able to
group by an attribute, or `runtime.jit` needs `method` as a key"*. Both are
outside this stream.

- **A key.** `KeyValue` is a closed enum in `src/event.rs` with twelve variants,
  none of which is a compiled method, and a thirteenth would have to be joinable
  — a key is what crosses sources. That is the larger change the scenario says
  it is, and `src/event.rs` is nobody's this wave.
- **Grouping by an attribute.** `Emit::TopK(u32)` is in `src/request.rs`, along
  with its parse and its `Display`. Expressing `--emit topk:10@method` means
  changing that enum, and `src/request.rs` belongs to no stream in this wave.

Building the emit half alone would be a function nothing calls, which clippy
refuses and which proves nothing. The exact three-file change is written up
under `## Needs from another stream`, with the note that **io 7 is the same
gap** — `io.bytes` measures a read/write split that no emit can show, for the
identical reason — so whoever takes one should take both.

What is *not* true is that this is a `topk` bug. `topk` ranks series correctly;
a series is a distinct set of keys; `runtime.jit` carries one key. Every number
in that output is right and the question is unanswerable, which is a different
failure and is why nothing here was changed to paper over it.

### net 6 — not a filter defect

`daddr` is declared as a filter field on `net.tcp.rtt` and the attribute holds
`35.190.46.17:443` — the address *and* the port. `==` against an address
therefore matches nothing forever, and `=~` works. Nothing in `src/filter.rs` is
wrong: `==` on a string is exact, which is the only thing it can be, and the
scenario document says so itself — *"either the attribute should be the address
alone or the caveat should say what it holds"*. Both are outside this stream:
the attribute is `src/collect/net/` (stream F) and the caveat is
`catalog/net.yaml` (stream B).

The first is the better fix and it costs nothing: `dport` and `sport` are
already separate numeric attributes, so the port inside `daddr` buys nothing and
costs the obvious filter.

A generic diagnosis was considered here and rejected — *this predicate kept
nothing, and the field it names is carried by N events whose values contain the
text* is a real and useful thing to say, and it is the natural next case after
`Filtered::unseen_fields`, which already exists for the same class of silence.
But it has to be computed in `apply_filter` in `src/collect/mod.rs`, before the
non-matching events are discarded, and it has to be carried on `Filtered`, which
is declared there too. Both are stream A's, and stream A is rewriting that file
this wave. The shape wanted is in `docs/open-findings.md`; it would close net 6
generically rather than for one attribute, and it is worth having whether or not
`daddr` is split.

### mem D4 — not an emit defect either

```
error: `mem.heap.live` cannot be collected for a pid (a preloaded shim can only be given
to a program at exec, so this source needs `-- <program>` rather than an already running
process) target yet
```

`CollectError::UnsupportedTarget { source_id, target }` formats as ``` `{source_id}`
cannot be collected for a {target} target yet ```, and `src/collect/mod.rs:702`
fills `{target}` with `format!("{} ({reason})", target.kind())`, where `reason`
is a whole sentence from `Prepared::requires_launch`. The sentence ends inside
a slot that holds one word.

Both files — `src/error.rs` for the variant and `src/collect/mod.rs` for the
site — are stream A's, and stream A is changing `CollectError` in this wave
anyway, since splitting `CollectError::Start` is its own exit-code fix. Nothing
in `src/emit.rs`, `src/filter.rs`, `src/encode.rs` or `src/render.rs` touches
this message. The variant wanted is written out in `docs/open-findings.md`.

---

## What this stream did not do, and why

**Nothing outside its four files.** Two of the six defects turned out to live in
files another stream is actively rewriting this wave — `src/collect/mod.rs` for
mem D4 and net 6's diagnosis, `src/collect/sched/offcpu.rs` for sched 1's other
half — so editing them would have produced a conflict rather than a fix. A third
lives in `src/request.rs`, which no stream owns.

**No measurement in these fixes needs a capability or a device**, so everything
above was exercised on this machine. The `io` scenario substitutes
`/proc/diskstats` through a mount namespace and skips loudly where it cannot;
that skip is unrelated to this change and was not introduced by it.

**`src/format.rs` was not touched.** Its claim about which formats can be
written is still crossed with reality by
`tests/cli.rs::every_declared_format_is_listed_with_whether_its_encoder_exists`,
which is the test the brief's trap points at, and by
`format::tests::profile_formats_refuse_emits_they_cannot_express` for the emit
matrix. Nothing here added, removed or re-shaped an encoder: `loss_headline` is
a change of wording inside three encoders that already existed, and the emit
matrix is untouched.
