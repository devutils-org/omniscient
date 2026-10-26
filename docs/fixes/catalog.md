# Stream B — what the catalog claims

Eleven defects, in `catalog/*.yaml` and `src/validate.rs`. Seven are fixed with
the scenario that found them now asserting the new behaviour; three turned out
to be something other than a catalog entry and carry a caveat instead; one is
left, with the one line that would close it.

The shape of the whole stream, in one sentence: **the catalog is not the only
place that decides what a request may ask for, and every defect here is a
collision between it and one of the others.** The mode × emit matrix in
`src/request.rs` refuses combinations the catalog offers; the selected backend
carries fewer keys than the entry declares; the emit layer ranks series where
the interesting axis is an attribute. Where the catalog was the one making a
promise nothing could keep, it stopped. Where the promise is reasonable and
something else refuses it, the promise came out anyway — a reader who is told
`topk` and cannot have it is worse off than one who is not told — and the exact
line that would bring it back is in `docs/open-findings.md` under "Needs from
another stream".

---

## Fixed

### mem D3 — two entries declared an emit no mode they declare can produce

`mem.sharing` declared `summary` with `sample` as its only mode, and `mem.swap`
declared `rate` with `poll` and `snapshot`. Both are refused on every machine,
in every build, by a rule that lives in another file — so a reader consulted
`omn list`, asked for what it printed, and was told no by something that never
names the entry.

Both emits are out. `mem.sharing` was the easy one: a summary over a sampled set
of HITM cache lines is close to meaningless, which is what the scenario already
said. `mem.swap` is a judgement — it measures a flow and a rate is the natural
thing to want — so the other fix, a `count` mode for it, is written up for
stream E; the emit comes back the day that lands.

```console
$ omn mem.swap --system -m poll@100ms -e rate -d 200ms
error: [mem.swap #1] `mem.swap` cannot emit `rate`
  hint: this source emits timeline, raw, summary
```

The refusal now names the source rather than the matrix, which is the message
that points at the line of YAML.

Asserted by `scenario_mem::every_emit_a_mem_source_declares_can_be_produced_by_a_mode_it_declares`,
which now requires the set to be empty and additionally checks that each of the
two is refused *by the source*. Against the old catalog both halves fail.

### net 1 — `net.bytes` declared `topk` and declares neither mode that produces one

Same shape, different judgement. The ranking is expressible — the collector
emits one event per interface, so the series exist — and what refuses it is the
matrix, not the data. `topk` is out of the entry because the catalog must not
promise what this build cannot serve, and `docs/open-findings.md` carries the
one-arm change to `ModeKind::valid_emits` that brings it back.

Asserted by `scenario_net::every_emit_a_net_source_declares_is_reachable_from_a_mode_it_declares`
(`RECORDED` is now empty, and the refusal must name the source) and by
`a_net_request_that_cannot_mean_anything_is_refused_with_the_axis_that_is_wrong`.

### gpu 3 — `rate` over a `ratio`

`gpu.engine_busy` reads occupancy, a ratio, and declared `rate`, which sums the
value attribute and divides by the window. Two readings of 0.030 became
`0.060/s`, for a device that was three per cent busy throughout.

The emit is out, and the entry gained the caveat that says why — occupancy does
not add up, so the mean and the maximum are the aggregates that mean something.
The rule is now enforced rather than remembered:

```rust
// src/validate.rs
fn adds_up(unit: Unit) -> bool {
    match unit {
        Unit::Ns | Unit::Bytes | Unit::Count | Unit::Joules => true,
        Unit::Ratio | Unit::Hz | Unit::Celsius => false,
    }
}
```

Written as an exhaustive match so that a unit added tomorrow has to answer the
question rather than inherit an answer. `hz` and `celsius` are in the same
class and had no offender; `ratio` had exactly one.

Asserted by `scenario_gpu::no_source_in_the_catalog_declares_a_rate_over_a_ratio`
— the same sweep over the whole catalog, now requiring none instead of one —
and by `validate::tests::a_rate_over_a_unit_that_does_not_add_up_is_a_broken_promise`.

### proc 5 — `proc.fd` declared the `file` key and no event carried it

The entry said `keys: [task, file]`, the validator accepted `--keys file` and
told a reader that "`proc.fd` carries task, file", and every event carries
`task` alone. A join written on `file` attributed zero events, which reads as a
workload that did nothing.

The entry declares `task`. The alternative — one event per descriptor, with a
`file` key — is what the entry's own caveat about walk cost refuses, and the new
caveat says so in as many words, so that the next reader does not have to
rediscover the trade.

Asserted by `scenario_proc::the_only_key_proc_fd_declares_is_the_only_one_its_events_carry`,
which replaces `defect_5_the_file_key_the_catalog_declares_is_on_no_event`: the
request is now refused by name, and the events that are produced carry exactly
the key that is left. Against the old catalog the first half fails, because the
request was accepted.

### io 2 — the mode's hint offered an emit the source refuses

```console
$ omn io.bytes --system -m count -e timeline
error: [io.bytes #1] mode `count` cannot produce `timeline`
  hint: in `count` mode the valid emits are rate, summary, raw     # before
  hint: in `count` mode `io.bytes` emits rate, summary, raw        # after
```

The hint listed what a mode can produce in the abstract, and the next line of
the same program refused one of them for this source: a reader who followed the
advice got a second refusal. It is now the intersection of the matrix and the
entry, so it can only ever name emits this source declares.

The empty intersection is handled too — it names both lists rather than
offering nothing — although `check_catalog` now makes it unreachable for the
embedded catalog.

Asserted by `scenario_io::the_mode_hint_offers_only_what_the_source_declares`,
which replaces `the_mode_hint_offers_an_emit_the_source_refuses`. It checks the
shape of the hint, that every emit it offers is one the entry declares, and —
the half that matters — that following it produces output rather than a second
refusal.

### io 7 — the read/write split was measured and no emit could show it

`io.bytes` puts `read_bytes`, `write_bytes`, `sector_bytes` and, where the
kernel reports them, `discarded_bytes` on every event, and declared
`[rate, summary, timeline, topk]` — none of which carries an attribute. So the
first question anybody asks of a volume, *was that reads or writes*, was
measured, attached to the event, and unreachable. It cost the scenarios
something concrete: a collector that credited every write as a read passed both
the read and the write workload, because both could only be checked against the
total.

`raw` is now declared on `io.bytes`, on `io.iops` — whose `ops` total hid
`reads`, `writes`, `discards` and `flushes` — and on `io.merges`, whose `merges`
total hid `read_merges` and `write_merges`. It needed no new mode: `raw` is
producible from `count`, `poll` and `trace`, which is all three of them.

```console
$ omn io.bytes --system -m count -e raw -d 1s
  raw
    88331025716875  io.bytes  dev:253+0  bytes=0 read_bytes=0 write_bytes=0 ...
    88331025716875  io.bytes  dev:259+0  bytes=81920 read_bytes=0 write_bytes=81920 ...
```

Each of the three entries gained a caveat naming the split and saying which emit
carries it.

Asserted by `scenario_io::the_split_a_write_workload_made_is_reported_as_writes`,
which is ground truth rather than shape: the same `fsync`ed 64 MiB the family's
other scenarios use, past `io.bytes -m count -e raw`, with `write_bytes`
required to carry it and each column capped by the same column of
`/proc/diskstats` over a window enclosing the run. Against the old catalog the
run is refused by name, so the scenario cannot pass there at all.

The first form of it asserted that a write workload reads nothing and had to be
rewritten, which is the trap `docs/dev/STATE.md` names in as many words: it passed alone
and failed inside the suite with exactly 67108864 bytes in the read column,
which belonged to the cold-read scenario running beside it on the same device.
Bracketing each column against the kernel's own is what survives a shared
machine; on a quiet one the read cap is near zero and the check is as sharp as
the first form was.

`the_mode_hint_offers_only_what_the_source_declares` covers the other half, by
running every emit the hint offers and requiring each to succeed.

### The check that stops the next one

Three of the seven above are the same defect in three families, found three
times by three threads. `src/validate.rs::check_catalog` is the check that
would have caught all three at load:

```rust
pub fn check_catalog(catalog: &Catalog) -> Vec<String>
```

Two rules today — an emit no declared mode can produce, and a `rate` over a unit
that does not add up — one complaint per broken promise, and
`validate::tests::the_catalog_promises_nothing_it_cannot_produce` runs it over
the embedded catalog. It guards what ships and nothing else: a catalog loaded
with `--catalog <dir>` is not checked, because the loader is
`src/catalog.rs` and this stream does not own it. The one line that makes it a
refusal is in `docs/open-findings.md`.

---

## Something else

Three of the eleven are real, and are not defects in `catalog/*.yaml`. Each
gained the caveat the catalog can honestly carry — caveats say what a number
does not mean, and reach the user with the request rather than in documentation
nobody reads at the moment it matters — and each is written up with the
signature that would close it.

### io 8 — a key the backend cannot produce is accepted silently

Filed as a catalog defect and it is not one: the chain of `io.bytes` really does
carry `blk`, `task`, `cgroup` and `dev`, and the entry is right to say so. What
is missing is the per-backend `keys:` the brief says to use — and it does not
exist. `Backend` has `targets` and `filter_fields` in exactly that shape and no
`keys`, so the data cannot be written at all: `#[serde(deny_unknown_fields)]`
refuses the key.

`docs/open-findings.md` carries the field, the two `Entry` accessors, the loader
check in both directions, and the resolver half — which is where the refusal has
to live, because which keys are real depends on which backend the resolver kept,
and the validator runs before that. Meanwhile `io.bytes` carries a caveat naming
which backend attaches which key and what a join over `task` will attribute
through the polled ones, which is nothing.

### net 5 — every aggregating emit of `net.iface` answers about errors alone

Not a catalog entry either: the collector's measured value is `errors`,
deliberately, because a bad frame and a full queue are different failures and
summing them would hide which happened. The consequence is that the separation
is made with attributes, which no aggregating emit carries, so a link dropping
every frame it is given reads `0 count/s` through `rate`, `timeline` and
`summary` alike.

One event per counter kind keeps the separation and makes both visible, and that
is a shape decision for whoever owns the collector. The entry now says what the
aggregates answer about and where the other counters are.

### runtime 1 — `topk` ranks processes, not the methods that were compiled

`topk` ranks series; a series is a distinct set of keys; `runtime.jit` carries
`task` alone. So the entry's `topk` is delivered literally and answers a
question nobody asked, and the question a JIT map exists to answer — which
methods dominated — is reachable through no emit, because `method` is an
attribute.

Removing `topk` from the entry would be wrong: a ranking over `--pid a,b,c` is
real, and the emit is not the thing that is broken. Adding `method` as a key is
the larger change and needs a `KeyKind` that does not exist. The caveat now says
what `topk` ranks here and what it does not, and names the two ways out; the
cheaper one — `topk` grouping by an attribute — is stream C's and is already
wanted by `io` and `net` for the same reason.

---

## Left

### sched 5 — `count` mode refuses `topk` on a run that has series to rank

One arm of `ModeKind::valid_emits` in `src/request.rs`, which is outside this
stream's paths and outside every other stream's too. The reasoning is already
written into that match for `raw` — a counted window is one event per series,
not one number — and `emit::topk` is `emit::rate` without the division, so a
counted run can be told its heaviest series' *rate* and cannot be told which
series was heaviest.

It closes sched 5, net 1 (the emit goes back into the entry) and the
`--emit topk` half of io 1. Written out in full in `docs/open-findings.md`.

Nothing in `catalog/sched.yaml` was changed for it: `sched.migrations` declares
`trace` as well, so its `topk` is reachable in principle and the entry is not
making a promise no build can keep. Deleting the mode or the emit to make the
refusal go away would have removed the capability rather than the defect.

### net 3 — `snapshot` on a counter source can only ever answer zero

Left in the catalog on purpose, with the contract written up for stream F.

Removing `snapshot` from `net.iface` and `net.backlog` is the change this stream
could make, and it is the wrong one twice over. It breaks
`examples/net-snapshot.yml`, which this stream does not own and which
`the_sessions_this_family_ships_say_something_the_catalog_offers` requires to
validate. And a snapshot of a counter is a question worth answering — the
totals since boot — rather than one worth refusing: `mem.swap --mode snapshot`
already answers exactly that, under the label "swap since boot" in
`examples/mem-machine-counters.yml`, and `io.pressure`'s caveat states the rule
for the whole catalog. The catalog will carry the caveat naming which of the two
measurements a reader is holding once there is a second one to name.

---

## What was touched outside this stream's defects

One assertion, declared here because the seam asks for it.

`scenario_gpu::a_count_mode_run_reports_a_window_longer_than_the_program_it_watched`
pins gpu defect 2, which is stream F's and is untouched. It measured the
over-long window through `--emit rate`, which gpu 3 removed from the source. It
now measures the same window through `--emit summary` — the window is reported
whatever the emit — and the half of it that checked the rate's arithmetic is
replaced by an upper bound on the window, so the scenario still fails if the
window stops being the program's life rounded up to the next tick.

---

## About this machine

`make check` is clean here, on the branch alone.

This machine has an NVIDIA device and `nvidia-smi`, so the `gpu` scenarios ran
rather than skipping: the two that were touched were exercised, not asserted.
The `io` scenarios that need a real block device ran as well. Nothing in this
stream needs `CAP_NET_RAW`, and `net.capture` was neither changed nor exercised.

One flake seen once, in a file this stream does not own and did not change:
`collect::tests::what_a_run_reports_spending_is_the_cpu_it_actually_burned`
failed in a `make check` run that shared the machine with a build, and passed
alone immediately after and in every run since. It is the species
`docs/open-findings.md` §11 and `docs/dev/STATE.md` both describe — two cpu-time readings
taken at different instants, pulled apart under load — and it is recorded rather
than worked around.
