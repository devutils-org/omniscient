# Stream S — the silent degradations

Five places where the tool accepted a request and then answered a different
question without saying so, filed at the tenth wave's close under "stream X"
(the bench) and "stream H" (hot attach) in `docs/open-findings.md`, plus the
three sources the brief said to check for the same shape and stream T's request
for a profile that follows threads.

**All five fixed, and the three checked sources and stream T's request with
them.** Every fix either makes the answer right or makes the refusal say why,
and each one has a scenario that fails against `main` (9317a3d) and passes
here — run once against each, with `main`'s `src/` and `catalog/` checked out
over this branch's tests, and the seven failures read before the sources were
restored.

---

## 1. `proc.syscall --filter 'syscall == 110'` dropped every call it pushed down

```console
omn proc.syscall -m trace -e summary --filter 'syscall == 110' -- <anything calling getppid>
  degraded: only `getppid` (110) was timed: the predicate on `syscall` was pushed into the program ...
  degraded: the predicate removed 2000 calls here and kept 0
```

The number was pushed into the program correctly, and then the collector's
userspace pass evaluated the same predicate against the event's `syscall`,
which is the **name**, and `"getppid" == 110` is false for every row.

**Fixed** in `src/collect/proc/syscall.rs`: the predicate the userspace pass
reads is rewritten once, at `Prepare::attach`, so that a *number* compared with
`syscall` is compared with `nr` — the attribute that carries the number the
kernel reported. Rewriting rather than skipping the userspace pass for a
pushed-down predicate is what keeps every position right: `syscall != 110`,
`!(syscall == 1)`, `syscall == 110 || syscall == 'write'` and a list mixing
numbers and names (`syscall in (110, 'write')`, which becomes
`nr in (110) || syscall in ('write')`) all mean the number.

- Unit: `a_syscall_number_is_compared_with_nr_in_userspace_wherever_it_sits`,
  eight predicates against a getppid row and a write row.
- Scenario: `tests/scenario_proc.rs::a_syscall_filter_by_number_counts_the_calls_the_program_made`
  — the filed command against a Python loop of 2000 `getppid`; exactly 2000 on
  this branch, 0 on `main`.

## 2. `proc.exec` accepted a filter and applied none — and so did `proc.exit` and `proc.signals`

```console
omn trace proc.exec --system -d 1s -m trace -e raw --filter 'filename == "/nonexistent/path"'
```

reported every exec on the machine. The mechanism is general: the resolver
credits every `bpf` backend with evaluating the predicate in the program
(`FilterPlacement::Kernel`), the runner's `apply_filter` then leaves its events
alone, and a collector that does not apply the filter itself applies it
nowhere. `proc.exit` and `proc.signals` had the same shape.

**Fixed** in `src/collect/proc/events.rs` for all three: the factories carry the
request's filter into the collector, and a `Predicate` applies it to every
drained record — before counting, for `proc.signals -m count` — counts what it
kept and removed, and says in the run's degradations that it was applied here,
over the drained records, and that every event was published and paid for.
None of the three programs can narrow on a string, so the whole predicate runs
in userspace.

`catalog/proc.yaml` gains `group_dead` as a filter field on `proc.exit`, with a
caveat: the tracepoint fires once per thread, and `group_dead == 1` is the one
event per process a reader counting programs wants. The events always carried
it; the filter could not name it.

- Unit: `predicate_tests::a_predicate_is_applied_counted_and_said`.
- Scenario: `tests/scenario_proc.rs::a_filter_on_the_machine_wide_proc_sources_keeps_only_what_it_names`
  — three observers (`filename` on `proc.exec`, `comm && group_dead` on
  `proc.exit`, `comm && sig` on `proc.signals`) under one workload: a shell
  under a name nothing else has, run a chosen number of times among execs of
  `/bin/true` every filter must remove. Exact counts on this branch; `main`
  reports `/usr/bin/rm` from another stream's suite.

**Not fixed here, filed** (the files are stream K's): `sched.wakeup`,
`sched.offcpu` and `sched.migrations` accept a filter the same way and apply
none — `omn sched.offcpu --system -d 300ms --filter 'comm == "nosuchcomm"'`
reports every task on the machine. Under "the eleventh wave, stream S" in
`docs/open-findings.md`, with the three factory lines.

## 3. `--backend force:` walked past a backend's missing probes

```console
omn trace mem.cache_llc --backend force:pmu -m count -e summary -- true
error: cannot start collecting `mem.cache_llc`: perf_event_open ... refused (No such file or directory)
```

`resolve()` returned as soon as the pin named a backend, without checking the
target or the probes it had just run: `--dump-resolved` printed a file with two
`missing` probes in it and accepted the request, and the run launched the
target and failed at attach.

**Fixed** in `src/resolve.rs`: a pin skips the walk, not the checks. A pinned
backend that cannot serve the target is refused with the walk's own reason
(`cannot attribute per pid: it would answer with a number scoped to system`),
and one whose requirements probe missing is refused with the probes' reasons —
both as `ResolveError::NoViableBackend`, so the exit code is 1, *this machine*,
and nothing is launched. A requirement no probe covers is still accepted, as it
is on the walk, and listed as unverified.

**One decision, and why.** A pinned backend that this build has *no collector*
for is still left to the runner's "no collector is written" (exit 3), even when
its probes are missing too. Both are true of `proc.exec --backend force:bpf:tp`
on this machine — tracefs is root-only here — and the build's gap is the one
that holds on every machine: reporting the machine's instead would send a
reader after privileges for a collector nobody wrote.
`scenario_proc::a_source_this_build_does_not_implement_exits_three_and_names_the_backend`
and `scenario_sched::pinning_a_backend_this_build_lacks_blames_the_build_and_not_the_machine`
both pin that answer, and both still pass.

- Unit: `a_pinned_backend_whose_requirements_probe_missing_is_refused_before_the_launch`
  (a real probe, `kernel>=999.0`, on a clone of the entry),
  `a_pinned_backend_that_cannot_serve_the_target_is_refused_with_the_reason`,
  `a_pinned_backend_this_build_lacks_is_left_to_the_build_answer_whatever_it_probes`.
- Scenario: `tests/scenario_mem.rs::a_pinned_backend_this_machine_cannot_serve_is_refused_before_the_launch`
  — the filed command with `--dump-resolved` (exit 1, no file printed) and with
  a launched program that would create a marker file (exit 1 from the resolver,
  the marker absent). It runs only where `omn list` says `pmu`'s descriptors
  are missing, which is this Zen 2, and skips loudly elsewhere.

`examples/bench/harness.rs` reads the probes out of the resolved file to avoid
this launch; it keeps working, since a refused `--dump-resolved` is a non-zero
exit it already treats as a refusal. Stream X can drop the workaround.

## 4. `mem.faults` declared `count` and refused it after the launch

**Fixed** by counting. `count` mode is now the kernel's
`PERF_COUNT_SW_PAGE_FAULTS` as a counting event, read through the `cpu`
family's counter group rather than a copy of it:
`collect::cpu::counters::PAGE_FAULTS` is a `Spec` outside `SPECS` (its source
and backend are registered by `mem`), and `page_faults::Prepare::attach` hands
a counted request to `counters::Prepare`. So it serves every target the catalog
declares — a process through one counter per thread (see 5), a cpu or the
machine through one per cpu — with the counter group's multiplexing and
never-ran handling. The events carry `count`, `counter`, `enabled_ns`,
`running_ns` and `comm`; there are no addresses, so a `count` run filtered on
`addr` keeps nothing, and the runner's userspace filter names `addr` as a field
no event carried.

- Scenario: `tests/scenario_mem.rs::a_counted_run_of_page_faults_is_the_kernels_own_count`
  — 64 MiB of `MADV_NOHUGEPAGE` pages touched by Python, against the program's
  own `getrusage` (minor + major): within −1 % to +5 %, the gaps being the
  faults between the fork and the gate (the program's, not the counter's) and
  the handful after its own read.
- `every_mode_and_emit_of_the_counter_sources_is_served_or_refused_by_the_right_code`
  no longer pins the three `mem.faults count` combinations as machine blame:
  they are served.

## 5. A per-task counter attached late counted one thread

A window over a phase that retired 2,000,000,000 instructions on a thread that
existed before the window opened counted **106,206**, with no loss and no note.
Inheritance is from the open: a counter opened on a running process follows
the thread it was opened on, and what that thread starts afterwards.

**Fixed** in `src/perf.rs`, with the better fix the request named rather than
the declaration: `ThreadCounters` opens one inheriting counter group per entry
of `/proc/<pid>/task` at the open and sums the readings (each scaled over its
own window first). At the launch gate there is one thread, so nothing changes
there. `Prepared::follows_only_later_threads` is **not** declared — the runner
would otherwise refuse a window the collector can now serve.

Used by `cpu.*` counters, `mem.cache_*`, `cpu.topdown` and `mem.faults -m
count`. A `--tid` target keeps meaning that task and what it starts
(`ThreadCounters::open_task`).

**What it still cannot do, and says.** A thread started between the listing
and the moment its creator's counter was opened is followed by nothing; one
started after that moment is followed by its creator's. From outside the two
cannot be told apart, and opening a counter on every such thread would count
the second kind twice. So they are not opened: they are counted after the
opens (`unlisted()`), and the run says how many appeared and that each is
counted only if its creator's counter was already open. On this machine that
has not been seen to be non-zero outside a thread storm; it is a note, never a
silent number. When more than one thread was opened on, the run says that too.

- Unit: `counters::a_counter_opened_on_a_running_process_counts_the_threads_it_already_had`
  (fails with the old single group: no count at all, the parked main thread
  never ran), `perf::following_tests::*`.
- **`tests/runner.rs::a_counter_attached_late_counts_the_threads_already_running_or_says_it_cannot`
  is ready to un-ignore.** Run with `--ignored` on this branch: *"the late
  window counted 2002089248 of the 2000000000 the second phase retired;
  attached: true"* — its first branch.

## The three checked sources, and stream T's profile

- **`cpu.profile`** — sampled the launched task only ("a sampling event is not
  inherited"). Now it follows every thread and every process the target forks,
  through `TaskSampler` in `src/perf.rs`: one event per task, `cpu == -1`, a
  ring each, opened on every task under the target at the start, and the tasks
  it starts found on each tick (`/proc/<pid>/task/<tid>/children`, walked
  from the root). A forked child is another address space, so `profile.rs`
  keeps one `Unwinder` per pid met and renumbers their stack ids into one table
  for the run. This answers stream T's request under "the tenth wave, stream
  T" -- though not with the `inherit` it proposed, and why is the one thing in
  this stream that was measured the wrong way first:

  **An inherited profile was built, and it broke a decomposition.** The first
  version used what `mem.faults` uses (below): one *inherited* event per
  thread per cpu, the events of one cpu sharing a ring, because the kernel maps
  a ring for an inherited event only when it is pinned to a cpu. It followed
  everything from its first instruction -- and
  `tests/join_engine.rs::a_program_s_life_is_a_whole_its_three_states_account_for`
  went red, repeatedly, where `main` passed three times in three: a residue of
  4 to 9 ms against a resolution of 2 to 5. Read row by row against the
  program's own `schedstat`, the blocked and runnable rows were exact and the
  on-cpu row was 0 to 5 ms short. A cpu-clock event keeps the unfinished part
  of its period when its task leaves the cpu, and resumes it only when the task
  comes back *to that event*: one event per cpu means a task that ran on
  twelve cpus can lose twelve periods of samples rather than one, while the
  decomposition states one period as the on-cpu row's resolution. That is an
  over-statement of precision, the direction the evidence ladder must never
  err in. A per-task `cpu == -1` event carries its period whole across cpus,
  so the profile went back to that shape, per task.

  What that shape gives up is a task's start: a thread or process the target
  starts is found by the next tick (10 ms), and what it did before is not
  sampled. It is not left unsaid: each task found late has its cpu time until
  then read from its own `schedstat` (the kernel's exact account), and the run
  says how many tasks were found late and how many milliseconds of cpu no
  sample covers. A task that starts and ends between two ticks is in no
  sample, and the note says that too. Rings of tasks that exited are released
  after their last drain, so a shell that runs a thousand programs does not
  hold a thousand rings.
  - Scenario: `scenario_cpu::a_profile_samples_every_thread_for_the_cpu_it_used`
    — three threads each burn 200 ms by `CLOCK_THREAD_CPUTIME_ID` and report
    their tid and clock; each thread's samples are its clock at 997 Hz within
    30 % and a sample. `main` has none for any of them.
  - Unit: `perf::following_tests::a_task_sampler_finds_a_thread_started_after_the_open_and_samples_it`.
  - Scenario: `scenario_cpu::a_profile_follows_the_process_its_target_forks`
    replaces `a_profile_that_cannot_see_the_work_says_it_did_not_see_it`, which
    asserted the disclosure of the old limit and *would* now fail — by design,
    its own message said so. The same shell forking its work, checked against
    `wait4`'s cpu time for the program and its reaped child.
- **`mem.faults`** (sample and trace) — the same shape, fixed with the
  inherited `ThreadSampler`: one inherited event per thread per cpu, sharing a
  ring of 128 pages per cpu, complete from each task's first fault. The
  unfinished-period problem above does not arise in `trace`, whose period is
  one fault; in `sample` a fault sample was already a statistical subset.
  - Scenario: `scenario_mem::the_faults_a_worker_thread_takes_are_traced_to_that_thread`
    — a Python worker thread touches 64 MiB of small pages and prints its tid
    and `getrusage(RUSAGE_THREAD)`; the faults traced to that tid are its own
    count, plus at most the tail after its own read (16391 against 16388 here).
    `main` traced 0 to it.
- **`sched.switches`** — the same shape (`SampledEvent::open_software` over
  `Scope::Process`, not inherited), in stream K's `src/collect/sched/switches.rs`.
  Filed with the replacement call, which is two lines:
  `ThreadSampler::open_process(&SampleSpec::software(PERF_COUNT_SW_CONTEXT_SWITCHES, 1, Callchain::Off), pid, RING_PAGES)`
  -- the inherited shape is right there, since `trace` samples every switch.
  `mem.ibs` already opens per thread per cpu on its own and does not need it.

## Also filed, not fixed

- **The resolved file still says `"placement": "kernel"` for every `bpf`
  backend**, and for `proc.exec`, `proc.exit`, `proc.signals` — and most of the
  `bpf` collectors of other families, which already applied the predicate
  themselves — it is userspace. The run's degradations now say where it ran;
  the resolved file cannot, because `placement()` knows the backend's id and
  not the collector. Filed: a backend field in the catalog saying which
  predicates a program carries, which needs `src/catalog.rs` (nobody's).

## Tests

- `cargo test --lib` for `resolve`, `collect::proc`, `collect::cpu`,
  `collect::mem::page_faults` and `perf`: green.
- `tests/scenario_cpu.rs`, `scenario_proc.rs`, `scenario_mem.rs`,
  `family_cpu.rs`, `family_proc.rs`: green, apart from the two `mem` tests
  below.
- The seven new scenarios, against `main`'s sources: all seven fail.
- `tests/join_engine.rs::a_program_s_life_is_a_whole_its_three_states_account_for`:
  red three times in three with the inherited profile, green three in three
  with the per-task one -- the measurement in "the three checked sources".
- `cargo test --workspace --no-fail-fast`: 1758 passed, 9 ignored. The
  failures, each looked at:
  - `scenario_proc::what_the_signal_ring_could_not_hold_is_counted_and_closes_against_what_was_sent`
    -- **this branch's, fixed.** Red twice in full runs with `late_ticks` of 3
    and 5: `proc.signals` had been made to build an event for every record
    before counting, and a counted storm of 300,000 signals then drained
    slower than its 10 ms tick. The event is built only when a predicate or
    `trace` reads it now; green five times since.
  - `scenario_cpu::a_counted_window_is_the_same_number_in_every_format_that_carries_it`
    (the `yaml` pass, once) and `scenario_net::a_connection_is_joined_from_its_handshake_to_its_close_by_the_cookie_alone`
    (once): each green alone on the first rerun.
  - `scenario_mem::a_gibibyte_of_reads_costs_one_l1_miss_per_cache_line`, at
    its TLB bound (70,351 and 65,592 misses against a bound of 65,536), and
    `scenario_mem::both_huge_page_backends_read_the_same_promotion_counter`:
    **the machine's, and red on `main` too** -- the second failed identically
    with `main`'s sources rebuilt on this machine, twice. Both assume huge
    pages the kernel is not granting right now: THP is `always`, but a
    fragmented machine under four streams' suites promotes what it can, and
    the TLB half of the first reads a mode, not whether the scan's buffer got
    huge pages. The fix belongs to the session (`examples/mem-cache-lines.yml`
    could print its own `AnonHugePages` from `smaps_rollup`, and the bound be
    asserted only when the buffer is mostly huge), which this stream does not
    own.
- `make check`: red only on the two `mem` tests above, which stop it before
  clippy; `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check`
  run separately: clean on this stream's files. **`cargo fmt --check` fails on
  `main` itself**, at `tests/scenario_runtime.rs:929` (an `assert!` that
  rustfmt wraps); `cargo fmt` rewrites it, and this branch leaves the file as
  `main` has it because it is not this stream's.

## Index rows

None: this stream adds no session to `examples/`. Every scenario above runs a
command line, which is the filed command the brief asked for.

## Ignored tests this half makes ready

- `tests/runner.rs::a_counter_attached_late_counts_the_threads_already_running_or_says_it_cannot`
  — passes on its first branch (the count), as shown above. Its `#[ignore]`
  reason names `Prepared::follows_only_later_threads`; the fix went the other
  way the request offered, one counter per thread, so the reason's text is
  stale as well as the attribute.
