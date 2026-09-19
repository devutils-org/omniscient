# Stream A — exit codes and the runner

What became of the twenty-seven defects `BRIEF.md` gave this stream, in three
groups. The files are `src/main.rs`, `src/cli.rs`, `src/collect/mod.rs` and
`src/error.rs`, plus the scenario assertions that named each defect.

**Twenty-three fixed, one found to be something else, three left** — two of them
a one-line change in a file this stream does not own, and one in the emit layer.

The suite is **1011 tests**, up from 993: eighteen scenarios were added or split
so that every fix below is asserted by the scenario that found the defect, and
would fail against the old behaviour.

---

## The four answers, and the one table they now come from

The tool has four things to say and had three codes, two of which were wrong.
`exit_for` in `src/main.rs` is the single place the mapping lives, and
`collect::Whose` is what the layers below it answer with:

| It says | Whose gap it is | Exit |
|---|---|---|
| `cannot be measured per …`, `mode X cannot produce Y`, `needs --mode` | the request's | **2** |
| `has no viable backend on this machine` | this machine's | **1** |
| `no collector is written for … yet`, and a collector refusing a shape it has not written | this build's | **3** |

The two that moved are rows two and three, and both are reached by a caller that
never reads the message: raising a privilege or changing machine fixes one of
them and nothing fixes the other, so a CI harness running a suite of sessions
across a fleet had no way to tell *fix your session* from *this is the wrong
machine for it*.

### How a refusal at attach time is classified

The subtle half, and the part the brief called the change. `CollectError::Start`
carried one sentence and no answer to *whose gap is this*, so every refusal from
a collector exited 1 — the code for a run that went wrong.

The rule that decides it needs nothing from the collector, and it is not a
guess: **a factory's build function is a pure function of the resolved
request**. Nothing has been launched, no counter has been opened and no file has
been read by the time it answers, so a refusal it produces cannot be about this
machine — it is this build's answer to the request as written, which is exit 3.
A refusal from `attach` or `start` happens against a real target and is a
failure of this run until the collector says otherwise.

That single rule moved six of the eight defects in this group. For the rest —
and for the handful of factory refusals that are really *"this cannot mean
anything"* rather than *"nobody wrote it"* — two helpers let a collector say so
from anywhere:

```rust
Err(collect::not_built("`count` mode for `pmu:sw` needs the perf layer's …"))
Err(collect::cannot_mean_anything("a queue depth counts over no window"))
```

They return a `String`, so adopting one is a one-line change at the call site
and no signature moves. The bit travels in front of the sentence as a control
character that `CollectError::start` takes off again before anything is shown;
`a_collector_can_say_whose_gap_a_refusal_is_without_showing_its_mark` is the
test that keeps it invisible. Widening `attach` and `start` to a typed error
would have been a change in fifty files across four streams to carry two bits.

---

## Fixed

### The exit codes — eight defects

| Defect | Was | Is | Asserted by |
|---|---|---|---|
| cpu 6 | `cpu.topdown` with no `slots` unit exited 2 | 1 | `each_refusal_names_the_thing_that_is_missing` |
| gpu 4 | `gpu.job` with no scheduler tracepoints exited 2 | 1 | `a_source_this_machine_cannot_serve_names_the_tracepoints_that_are_absent` |
| io 3 | `io.pressure` on a kernel without PSI exited 2 | 1 | `a_machine_that_cannot_serve_pressure_says_which_file_it_wanted` |
| net 8 | `net.capture` without `CAP_NET_RAW` exited 2 | 1 | `a_machine_that_cannot_serve_a_net_request_names_what_it_is_missing` |
| proc 4 | a requirement no machine meets exited 2 | 1 | `a_machine_that_cannot_serve_a_valid_request_has_its_own_exit_code` |
| power 2, row 2 | `power.throttle` without `CAP_BPF` exited 2 | 1 | `every_axis_this_family_declares_is_served_or_refused_by_name` |
| cpu 4 | `cpu.cycles -m sample` exited 1 | 3 | `a_mode_the_build_does_not_implement_exits_as_unimplemented` |
| io 1 | `io.bytes -m trace` exited 1 | 3 | `each_refusal_names_the_thing_that_is_actually_missing` |
| net 7 | `net.hwts -m trace` exited 1 | 3 | `the_trace_leg_of_the_sweep_answers_or_names_the_backend_it_would_need` |
| power 2, row 4 | `power.freq -m trace` exited 1 | 3 | `every_axis_this_family_declares_is_served_or_refused_by_name` |
| mem D5, 13 of 16 | the three cache sources' `sample` and the two heap sources' `snapshot` exited 1 | 3 | the four `every_mode_and_emit_of_the_*_sources_…` sweeps |
| sched 6 | `sched.switches` with no `--mode` exited 1 | 2 | `an_under_specified_request_is_rejected_rather_than_failed` |

The hint printed under a resolver failure is per case now. It used to say *"the
request is valid; it is this machine that cannot serve it"* under every one of
them, including a source that is not in the catalog, one line above the code for
a request that is not valid at all.

Two smaller things fell out of the same table. A refusal from the parser — a
flag that does not exist, a value that does not parse, two flags that cannot
both be meant — now exits 2 rather than 1: it is the same answer the validator
gives one layer in, and `--mode` misspelled and `--mode` missing used to exit
differently while being the same mistake. And `--help` and `--version` still
exit 0.

### The loss reasons — five defects, one site

cpu 8, gpu 5, io 5, net 2 and proc 6 are five families finding one line of
`drive()`. `Losses::reasons` maps a reason to how often it happened, and the
late-tick reason carried the nanoseconds of the round that overran, so no two
were ever equal: every occurrence was its own reason with a count of one.
Thirteen hundred lines for a two-hundred-millisecond run, 80 KB of JSON for
three hundred milliseconds of gpu polling, and the map grew without bound for
the length of the run — at exactly the moment it was least readable.

A reason and its measurement are two things. `Losses::late_tick` takes the
overrun and the tick and keeps them apart: the reason is the *kind*, and the
magnitudes accumulate in `Losses::overrun`, a new field carrying the tick, the
worst single overrun and the total. `Losses::summarize`, called once when
everything has been absorbed, joins them back into the one line a reader sees:

```
LOSS: 0 dropped, 0 failed reads, 369 late ticks
  369x a round of reads outran the 10.0us tick: worst 218.3us, 135.3us on average
```

The question a reader actually had — *how late were they* — is answerable now,
and it was not before, because each answer was its own key.

Asserted by `a_run_that_was_late_many_times_explains_it_once` (cpu),
`the_loss_report_carries_one_reason_for_all_the_late_ticks` (gpu),
`a_poll_faster_than_the_file_can_be_read_counts_every_late_tick` (io),
`a_poll_faster_than_its_own_read_counts_every_late_tick` (net),
`a_late_tick_is_one_reason_with_a_count_rather_than_one_reason_each` (proc), and
`late_ticks_are_one_reason_carrying_the_count_they_explain` in `src/collect/`.

### The runner's pace — five defects, one seam

io 6, net 4, power 3, power 4 and proc 3 are the same thing seen from five
sides. `run_all_within` took **one** interval for the whole run: the smallest
tick any collector declared, and where none did — which is every poller in
`io`, `mem`, `power` and `proc` — the mode of `resolved[0]`, whichever request
happened to be written first. So:

- `poll@1s` written first served `poll@10ms` at one second, and the other way
  round served the slow one at ten milliseconds;
- a `count` request, which names no cadence, imposed its one-second fallback on
  everything after it;
- a `snapshot` was a single read only when *every* request was one, so a
  snapshot written beside a poll became a series;
- and `Mode::Snapshot`'s fallback interval of zero, applied to the whole
  session, turned a 400 ms window into 5257 rounds against procfs reported as a
  normal run with no loss.

Request order decided another request's sampling rate, which is the one thing a
declarative session must not do.

Each request is now driven by its own `Pace`, from its own mode and its own
collector. A collector that declares a tick keeps it — a ring drained less often
than it fills loses events, and nothing a request says makes that acceptable —
and a collector that declares none is read at the cadence the request named. A
snapshot is one read wherever it is written. The loop wakes at the earliest
moment any collector falls due, so a run is no less responsive to its stop
condition than when one interval governed everything.

Two consequences worth knowing. An interval of zero is *"as fast as this can be
read"*, which is not a cadence and cannot be fallen behind, so it no longer
manufactures a late tick per round — that alone was 158 of power 3's reported
losses, in the one direction this project promises never to fail in. And a
collector on a slow cadence is no longer read at the end of the window: its
series is the intervals it was asked for, with no short final one to drag every
summary computed over it, which is the same reasoning `Collector::stop` already
carried.

Asserted by `request_order_does_not_decide_another_requests_rate` (io),
`a_snapshot_beside_a_poll_is_still_a_single_read` (net),
`a_snapshot_beside_a_poll_is_one_read_and_invents_no_losses` and
`two_requests_at_two_rates_are_each_served_at_their_own` (power),
`every_request_in_a_session_keeps_the_rate_it_asked_for` (proc), and
`each_request_is_paced_by_its_own_mode_and_its_own_collector` in
`src/collect/`.

### mem D7 — a sampling request answered with a single poll

`mem.numa -m sample@997hz` resolved onto the poll-only `fs:proc` backend, read
`numa_maps` once, and reported success. A thousand samples a second asked for,
one read taken, and the only degradation in the output was about placement
versus accesses — true, declared, and about something else entirely. What made
it worse than a missing number is that the number it produced was *plausible*.

The runner names it. A request in `sample` or `trace` mode whose collector
declares no cadence of its own is being read at a poll interval nobody asked
for, and that is a degradation of the request rather than of the measurement, so
it travels with the results like every other one:

```
degraded: `mem.numa` was asked for in `sample@997hz` mode and `fs:proc` has no
          sampling of its own: it was read every 1.00s, which is a poll and not
          the rate that was requested
```

The signal is exact rather than heuristic: a collector that samples has a ring
to drain and therefore declares a tick; one that reads a file does not.

`mem.md` says the resolver should not have selected a poll-only backend for a
sampling request in the first place, and that is right and is `src/resolve.rs`,
which this stream does not own. It is written up under "Needs from another
stream" in `docs/open-findings.md`. What is here is the other half of the answer
the scenario accepts: a run that says which part of the request it could not
honour.

Asserted by `a_sampling_request_that_no_backend_can_sample_is_not_answered_with_a_poll`.

### sched 8 — a session reported all its rejections and only its first missing collector

`examples/sched-forced-backends.yml` has five requests with no collector and
printed one, then stopped; `examples/sched-refusals.yml.rejected` has four
invalid requests and prints four. Same shape of file, same kind of answer, two
behaviours, and a reader fixing a session learned about its problems one run at
a time.

`collect::all_or_nothing` asks every request and collects every refusal;
`CollectError::Several` carries them and renders them one per line. Where they
disagree about whose gap they are, the code is the one for the work the reader
has to do first: a request that cannot mean anything has to be rewritten before
a missing collector is worth chasing, and both come before moving machine.

Asserted by `pinning_a_backend_this_build_lacks_blames_the_build_and_not_the_machine`
and `several_refusals_are_reported_together`.

### cpu 5 — `--duration` beside `-f`

It was accepted and discarded: the session named no duration, the command line
named 100 ms, the run lasted as long as the workload did, and `--dump-request` —
the thing a reader checks when they want to know what they asked for — carried
no duration at all. Where the session *did* name one, the file won and the flag
was silently ignored. `cpu.md` is right that either answer is defensible and
that being told is not optional: a flag that leaves no trace is the silent case
this repository refuses everywhere else.

The rule that settles it is one a `Session` already describes. The command line
may replace what a session holds **for the whole of itself** — `duration`,
`budget`, `format` and `out`, of which the last two already worked that way.
Every other axis names one request, and a file may have many, so the parser
refuses those beside `-f` by name rather than accepting and dropping them:

```console
$ omn -f examples/cpu-known-work.yml -m count
error: the argument '--file <PATH>' cannot be used with '--mode <MODE>'
$ omn -f examples/cpu-known-work.yml -d 100ms --dump-request
note: --duration 100ms applied to every request of this session
```

The replacement reaches the requests as well as the session, because
`Session::apply_defaults` has already run by the time the file is loaded:
setting the session field alone would leave every request holding the file's
value and change nothing, which is how this was silent in the first place.

Asserted by `a_duration_beside_a_session_file_replaces_the_sessions_own`.

### cpu 10 — a structured run shares stdout with the program it launched

A launched program inherits this process's stdout, which is right: its output is
the user's and swallowing it would be worse. But a structured format is one
document, so anything the target prints lands in the middle of it and
`omn … --format json -- ./prog | jq` works exactly until `./prog` says
something — arriving as a parse error in whatever consumed the pipe rather than
as anything about this tool.

The way out already existed and the run already named it; what was missing was
being told first. It is a warning rather than a refusal, because the run is
legitimate and redirecting the program's output is the other way out. Text is
left alone: it is a report for a person, who can see what came from where.

Asserted by `a_structured_run_that_launches_a_program_says_they_share_the_stdout`.

---

## Something else

### mem D5 is two defects, not one, and only one of them was where it looked

Sixteen combinations exited 1 for a reason that has nothing to do with the
machine. Thirteen of them are factory refusals and are fixed by the rule above.
The other three — `mem.faults` in `count` mode, with `rate`, `raw` and
`summary` — are not the same defect at all: `mem.faults` refuses from
`Collector::start`, against a target that already exists, at a point where a
refusal is genuinely ambiguous between *a mode nobody wrote* and *a counter that
would not open*. No rule the runner can apply tells those apart, and inventing
one would have been the guess this project is built against. It is a one-line
change in a file stream E owns; the signature is in `docs/open-findings.md`.

The same is true of sched 9 (`sched.switches --mode count`), which is stream D's
`src/collect/sched/switches.rs`.

### The `io` factory also refuses two things that cannot mean anything

`io.inflight -m count` ("a queue depth is read at an instant, so it counts over
no window") and `io.bytes -m snapshot` ("a counter that has been rising since
boot, not what it did during a run") now exit 3 rather than 1. Neither is
listed as a defect and neither is right: they are requests that cannot mean
anything, which is exit 2, and the honest place to say so is the catalog — an
entry declaring a mode no backend of it can serve. Stream B owns that. Until
then 3 is not worse than the 1 they had, and `cannot_mean_anything` is there
for whoever gets to it first.

---

## Left

### sched 9 and the `mem.faults` third of D5 — one line each, in another stream's file

Both are `Collector::start` refusing a mode this build has not written, where
the runner cannot tell that from a counter that would not open. The change is
the same in both places and is written out in `docs/open-findings.md` under
"Needs from another stream".

### sched 7 — `--keys` is accepted, recorded, and not used

`SeriesKey::of` takes the first key the collector happened to attach and no
collector anywhere reads `resolved.request.keys`. `sched.md` says as much: "this
is not a `sched` defect — no collector anywhere reads the request's keys". It is
`src/emit.rs`, which is stream C's, and nothing in this stream's files can
reach it. Silently substituting one of the five axes the tool advertises is
worse than refusing the request, and it is worth someone's wave.

### `man/omn.1` and `GUIDE.md` now document exit codes that have changed

Both carry the table this stream rewrote and neither is in this stream's paths.
`man/omn.1` documents exit 2 as *"it cannot mean what it asks for, or this
machine cannot serve it"* and exit 1 as *"usage, parsing, or a failure while
running"*; `GUIDE.md` §"The refusal" has the four-row table with the two wrong
codes in it, and a paragraph below explaining that two of them are wrong. All
three want the coordinator. The exact replacement text is in
`docs/open-findings.md`.

A documented contract that has quietly stopped being true is the same kind of
lie as a wrong number, so this is the item on this list worth doing first.

---

## What this machine could and could not exercise

Every fix above was run on this machine, with `CAP_BPF` and `CAP_PERFMON` on
`target/debug/omn` — `make` reports granting them on every build here. `make
check` is clean: 1011 tests, clippy with `-D warnings`, and `cargo fmt --check`.

Two things were *not* exercised here and are not claimed:

- **`net.capture` still needs `CAP_NET_RAW`, which this build does not grant.**
  Its refusal now exits 1, and that path is asserted — it is the refusal that is
  reachable here, not the capture. What a successful capture does with the new
  pacing is untested on this machine.
- **`gpu.job` needs an AMD or Intel scheduler's tracepoints.** The exit-code
  assertion runs here because the refusal is what this machine produces; a
  machine that has them takes the other branch and measures.

### Three flaky tests seen from here, none of them in this stream's files

Recorded because whoever merges eight branches will meet them, and because each
passes alone and fails under the load of the whole suite — the species
`docs/open-findings.md` §11 describes.

- `tests/family_sched.rs::tasks_sharing_one_cpu_wait_the_share_the_scheduler_owes_them`,
  about one run in five, taking `the_kernels_buckets_are_the_emit_layers_buckets`
  with it through a poisoned mutex. This is sched 10, already recorded, and it
  reproduced here unchanged.
- `src/collect/cpu/counters.rs::a_cache_source_publishes_the_accesses_its_misses_are_out_of`,
  about one run in eight under `cargo test --lib`: the counter group comes back
  with no events at all when the pmu is crowded by the rest of the suite. Same
  species as power 5, in a different file, and stream D's.
- `tests/scenario_gpu.rs`'s two device-value tests failed once together on a
  loaded machine and passed on every run since; both read a number off a device
  the rest of the suite keeps awake, which their own file documents.

None of the three changed behaviour under this stream's changes: each was seen
failing and then passing with the same binary.
