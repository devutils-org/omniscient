# Hot attach — a request whose window is its own

`ROADMAP.md` phase 6, task 4: *a collector can be added and removed
mid-session*, without restarting the run or breaking in-flight joins. This is
the tenth wave's stream H, and this document is its handoff.

What works: a request carries a `window`, the runner attaches and detaches it
at those moments while every other request keeps running, and each collection
reports the span it watched. Two sources attached for one phase each of a
program with a known instruction count count that phase to within +0.17 %, and
nothing of the other phase. What does not work yet is in three files other
streams own: the join engine credits events after a detach to a stale state,
the results document cannot yet say where the window was, and a per-task
counter attached late to a program that already has threads counts one of
them. Each is filed with its signature in `docs/open-findings.md` under
"stream H".

---

## The surface, and why it is this one

```yaml
requests:
  - label: first phase
    source: cpu.instructions
    target: &program {launch: [./two-phases]}
    mode: count
    window: ..1200ms        # attached with the rest, detached at 1.2 s
  - label: second phase
    source: cpu.instructions
    target: *program
    mode: count
    window: 1800ms..        # attached at 1.8 s, kept to the end
  - label: whole
    source: cpu.instructions
    target: *program
    mode: count             # no window: the whole run
```

**A `window` on the request, relative to the run, written `<start>..<stop>`
with either end open.** It is part of `Request`, not of the session, for the
reason the brief gives: the file and the command line build the same
`Request`, and a window kept beside it would be a second description the
command line could not produce. It is one string rather than a map so that it
reads the same in YAML, JSON and TOML and is already the syntax of a
command-line flag (`--window 1s..3s`, filed for `src/cli.rs`). `0s..2s` and
`..2s` watch the same span and are kept apart in the dump, because the file is
a record of what was asked.

"Relative to the run" means relative to the moment the accounting opens: every
collector without a late start is attached, and the launch gate has just
opened. That is the one instant every request of a session shares, and it is
where the run's `observed_ns` has always been counted from.

What a window is refused for, before anything is launched:

| The window | Refused because | Exit |
|---|---|---|
| any, on a source served at exec (the preload shim) | it exists from the program's first instruction to its last: it cannot be given to a running program, and detaching it early would leave the program writing into a pipe nobody reads, which blocks or kills it | 3 — a uprobe or USDT backend in the catalog could honour it; this build has none |
| a stop on a `snapshot` | a single read has nothing to detach | 2 |
| an end past the run's duration | it would quietly become a shorter window, or none, under the label that asked for more | 2 |
| `3s..1s`, `2s..2s`, `..` | it closes before it opens, or it is the run's own | 2, at parse |

A session whose every request has a window that closes needs no duration: the
run ends when the last one closes ("every request's window had closed").

## What the runner does at each edge

`src/collect/mod.rs`. A request is a `Slot`: `Waiting` (prepared, not
attached), `Attached`, `Detached` (the collector is dropped, and what the
results need of it -- its value attribute, its stacks, its notes -- is kept),
or `Missed`. Factories are asked before the launch for every request, windowed
or not, so "no collector is written for this" is still the first answer.

**At a window's start**, at the top of a round: the request's account is
opened *before* the attach, then the prepared collector is attached to the
bound target and started, and its first read is taken in that same round. A
counter's baseline is its attach, not one interval after it. What the attach
took is recorded (`Watched::attach_ns`) and charged to that request.

**At a window's stop**, at the bottom of a round: the collector is due at that
moment whatever its pace, it is read, then stopped, its trailing events and
its own losses are absorbed, and it is dropped -- releasing what it held in the
kernel is the point of detaching. This is the edge convention the runner
already had for the end of the run ("read first, then decide"), applied at a
second place.

**A late attach that fails** is a loss of that window (`failed_reads`, with the
reason naming the source), and the run goes on without it. A window that opened
after the run had already ended was never attached, and says so in
`stopped_because` and in a note rather than presenting an empty series.

Every `Collection` carries a `window: Watched` -- `run_opened_ns`,
`opened_ns`, `closed_ns`, `attached`, `attach_ns` -- and `observed_ns` is its
span. For a request without a window that is the run's own span, unchanged.

## Scenarios

| Scenario | Ground truth | Measured |
|---|---|---|
| `tests/runner.rs` `a_source_attached_for_one_phase_counts_that_phase_and_nothing_of_the_other` | a 772-byte static ELF written by the test: one thread, no loader, 2 instructions per turn; 158 user-mode instructions besides its loops by `perf stat -e instructions:u`. 1,000,000,000 in the first phase, 2 s asleep, 2,000,000,000 in the second; the program reports its own phase boundaries | first window 1,001,581,525 (+0.16 %), pause 0, second 2,001,740,370 (+0.09 %), whole 3,003,321,736; the three windows tile the run to within 10,000. Stable over three runs |
| `tests/runner.rs` `a_probe_attached_mid_run_sees_the_second_phase_and_nothing_of_the_first` | the same ELF: 40 sleeps, a 1.5 s pause, 25 sleeps; a wakeup each | `sched.wakeup` attached at 1 s through `bpf:raw_tp`: 26 wakeups of the program (the pause's and the 25), none of the first phase; the whole run 66 in the program's life plus the launch gate's |
| `tests/budget.rs` `a_request_attached_for_part_of_the_run_is_weighed_over_its_own_window` | a shell loop burning one cpu for the whole run | a `250ms..500ms` window weighed over 250.5 ms of wall and 250 ms of the program's cpu, against 760 ms and 756 ms for the whole run |
| `src/collect/mod.rs` `attaching_mid_run_is_paid_for_by_the_request_that_attached` | 30 ms of cpu injected into the attach | the attach timed at ≥ 30 ms and in the request's own account |
| `src/collect/mod.rs` `a_window_is_attached_and_detached_while_the_rest_keep_running` | the collectors' own record of when they were read | every read inside the window, the first at the attach, the last at the detach; the collector beside it read throughout |
| `src/collect/mod.rs` `a_late_attach_that_fails_is_a_loss_and_the_run_goes_on`, `a_detached_collector_s_losses_are_its_own`, `a_run_ends_when_every_window_has_closed`, `a_windowed_request_s_collection_states_its_own_window` | a refusing attach, a collector reporting 7 drops, two stops, `mem.rss` on this process | as named |
| `tests/runner.rs` `a_preload_source_given_a_window_is_refused_by_name_before_the_run` | a program whose first act is to create a file | `mem.heap.alloc` with `500ms..` and with `..500ms`: exit 3, the source, the window and "at exec" named, and the file never created |
| `tests/runner.rs` `a_window_the_run_cannot_hold_is_refused_with_the_reason`, `a_session_with_windows_dumps_unchanged`; `src/session.rs`, `src/request.rs` | — | refusals as tabled above; a windowed session dumps, reads back and dumps identically, through YAML, JSON and TOML |
| `tests/runner.rs` `a_join_across_a_detached_source_does_not_count_what_fell_after_it_as_a_loss` | the ELF's 40 and 25 sleeps | **ignored**, waits on `join.rs` — see below |
| `tests/runner.rs` `a_counter_attached_late_counts_the_threads_already_running_or_says_it_cannot` | the second phase retires 2,000,000,000 on libtest's test thread | **ignored**, waits on `counters.rs` — see below |

Two sessions: `examples/attach-one-phase.yml` (four counters, three windows
tiling one run) and `examples/attach-late-probe.yml` (an eBPF probe loaded at
1 s: 21 wakeups of the program against 42 for the whole run, "attaching it took
1.2ms while the target ran").

## What the scenarios found

**1. A per-task counter attached late counts one thread and says nothing.**
The largest finding. `cpu.instructions` opens its group on the pid with
`inherit`, and inheritance is from the open. At the launch gate that is every
thread the program will start. Attached by a window to a program that already
runs two threads -- libtest runs its test on a thread of its own -- it counted
**106,206** instructions over a phase that retired 2,000,000,000, with a clean
loss line. The runner now has the hook to stop this
(`Prepared::follows_only_later_threads`: when a collector declares it and
`/proc/<pid>/task` has more than one entry at the attach, the window is not
attached and its loss says why), and the declaration is filed for
`counters.rs` -- which is also the place for the real fix, one counter per
existing thread. Until then the ground-truth scenario uses a single-threaded
program, and says why. The same blind spot applies to a budget that moves a
collection onto a cheaper backend mid-run: that attach has always been late.

**2. A join across a detached source credits a stale state.** Filed for
`join.rs` with the field and the entry point. With `sched.wakeup` detached at
1 s and `sched.switches` traced throughout, the join reports 67 matched and 0
unmatched: the last wakeup seen before the detach stays open, and every block
after it is credited to that wakeup (`task:0 (idle) -> task`, 63 matches,
still open). The brief expected these to be miscounted as losses; they are
worse, they are attributed. The join needs to know each source's window, and
the collections now carry it.

**3. A counter over a window where the program slept has no count.** A
per-task counter is enabled only while its task runs, so the pause window of
`examples/attach-one-phase.yml` reports "enabled for no time at all: the target
did not run during the window, so there is no count, which is not the same as a
count of zero" -- as a failed read. That is `counters.rs` being honest, and it
is kept; it means a window over an idle phase reads as a loss rather than as
zero, and a reader should expect that.

**4. A window's cost is only as fine as the meter.** The overhead's denominator
for a launched program is `/proc/<pid>/stat`, in 10 ms ticks, until the reap.
A window over a program that burns little -- a python script asleep after 13 ms
of startup -- reads a tick either way at its attach, so its "target's cpu" can
be the whole run's. A window over a busy program is exact to the tick, as the
budget scenario shows.

**5. A request's own `duration` is still not a window.** It was already
possible to give one request `duration: 10s` in a 30 s session, and the run
has always taken the longest and ignored the rest. That is now expressible
properly (`window: ..10s`), and the old field is left as it was: changing what
an existing session measures without its author asking is the silent change
this project refuses. Whether a per-request `duration` shorter than the run
should be refused, pointing at `window`, is a decision for the coordinator.

## What is not done, and where it is

| What | Blocked on | Filed |
|---|---|---|
| a join that knows a source's window | `src/join.rs` (`Correlation::outside_window`, `apply_within`), then `src/main.rs` passing the windows | yes, with signatures |
| the window's placement in the results document | `src/emit.rs` (`Results::window`); until then it is a note and `stopped_because` | yes |
| a late counter on a threaded program | `src/collect/cpu/counters.rs` declaring `follows_only_later_threads`, or opening per thread | yes |
| `--window` on the command line | `src/cli.rs`, `man/omn.1` | yes |
| `DECISIONS.md`'s concurrent-requests line | the coordinator; what it should say is in the filing | yes |
| the escalation (phase 10) | the next wave, standing on this | — |

**The concurrent-requests line in `DECISIONS.md`.** It says a session with
several requests is refused until hot attach lands. That has not been true for
several waves: a session runs any number of requests in one window, one
workload beside any number of machine targets, and each now has its own window
inside the run. What is still refused is two *workloads*, and hot attach does
not change why: whose exit ends the run, which program a launch means, and
what an overhead is a percentage of each still have two answers. The line
should say that.

## Files outside this stream's list

`Request` gained a field, and a struct literal does not compile without it. The
five literals outside this stream's files each got one line, `window: None`,
and nothing else: `src/cli.rs`, `src/resolve.rs`, `src/render.rs`,
`src/validate.rs`, `tests/catalog_model.rs`. Stream T's `src/triage/` will need
the same line in any literal it adds.

---

## The suite

`make check` once, before the commit. It stopped at the first red binary, so
one `cargo test --workspace --no-fail-fast` followed: **1636 passed, 3 failed,
2 ignored** (the two scenarios above that wait on other streams). Clippy
(`-D warnings`) and fmt are clean.

| Test | Why it is red | Alone |
|---|---|---|
| `aggregation::two_emits_of_one_request_describe_the_same_events` | a float compared exactly; red on `main`, filed by stream J in the eighth wave | fails, as filed |
| `cli::the_session_indexes_hold_exactly_the_sessions_on_disk` | `attach-late-probe.yml` and `attach-one-phase.yml` are not in the indexes yet | expected: rows below |
| `scenario_mem::a_gibibyte_of_reads_costs_one_l1_miss_per_cache_line` | the TLB counter saw 68,638 misses with huge pages set to `always`; recorded in `docs/scenarios/cross-machine.md` with 69,689 | failed once alone right after the suite, then passed three times on this branch and three times on `main` (`6af667b`), interleaved: load on huge pages, not this change |

`tests/runner.rs` (18 passed, 2 ignored), `tests/budget.rs` (12) and
`cargo test --lib collect::tests` (33), `request::tests`, `session::tests`
passed in every separate run; the two ground-truth scenarios three times in a
row.

---

## Index rows, ready to paste

`examples/README.md` and `GUIDE.md`, in a new `attach-` section:

```
| `attach-late-probe.yml` | Does a kernel probe loaded mid-run see what happened after it and nothing before? |
| `attach-one-phase.yml` | Does a source attached for one phase of a program count that phase and nothing of the other? |
```
