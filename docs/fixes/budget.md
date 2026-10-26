# Stream R, thirteenth wave — what a run costs the target

Finding 47 said it in one line: `proc.syscall` in `count` costs 0.42 % of the
target's cpu in `cost.json` and **+296 %** in the target's own timed section,
because the eBPF program runs on every syscall in the target's context. The cost
this tool reported and the budget it held were the tool's own cpu, and for an
in-kernel or in-process collector that is not where the cost is.

**What landed**: the cost a collector spends inside the target is measured or
said to be unknown, never zero, and `--budget` weighs it (1); the bands the
bench's dilation contradicts moved, and the rule is written (2); losses per
ring reach the series they belong to, as far as the collector layer and the
emit layer go (3); what the shim stream holds is bounded, and the storm of
finding 47 accounts for every acquisition (4); the filed requests, each checked
first (5); and five of the six tally rows, the sixth already landed (6).

**Left**: the per-ring half needs `cpu.profile` to report its drops per ring
(stream P's file), and its scenario says so loudly until it does; the eBPF half
of the ground truth cannot be created on this machine, where the kernel is not
timing programs and this tool may not ask it to, and says so with the numbers.

---

## 1. The cost inside the target: measured, or unknown

`collect::Overhead` gains three fields, carried into `cost.json` and now into
the results themselves:

| field | what it is |
|---|---|
| `in_target_cpu_ns` | cpu the collector's own code spent in the target's context. `0` for a collector that reads from outside; **`null` when it runs inside and the run cannot know** |
| `in_target_runs` | how many times that code ran, when it is counted |
| `in_target_from` | where the number came from, or why there is none |

`Overhead::percent()` is now `(tool's cpu + in-target) / (target's cpu −
in-target)` -- the target's account already holds what ran in its context, so
the denominator is the work it did of its own, and the quotient is the dilation
a program that times itself sees (300 % for a program four times slower, not
the 75 % of its cpu the tracing was). `own_percent()` is the old number,
recorded beside it in `cost.json`. Where the in-target part is unknown the
percentage is the tool's alone, and a ceiling the known part stays under is
reported as **not checked whole** (`within_budget: null`, "could not be checked
whole") rather than held. Over the ceiling on what is known is over it.

**The mechanism is chosen by the backend's mechanism**, in
`InsideMeter::of` (`src/collect/mod.rs`):

- **eBPF** (`bpf`, `bpf:*`): the kernel's own `run_time_ns` and `run_cnt` of the
  programs the request loaded -- the program descriptors that appear in
  `/proc/self/fdinfo` while that request is prepared and attached, read by
  descriptor and checked by program id, so whoever loaded them (the fentry
  trampolines load their own, outside `src/bpf/syscall.rs`) does not matter. The
  kernel counts only while `bpf_stats_enabled_key` is on: the run turns it on
  with `BPF_ENABLE_STATS` and holds the descriptor, or uses the sysctl if it is
  already 1. **`BPF_ENABLE_STATS` needs `CAP_SYS_ADMIN`**, which the `CAP_BPF`
  this tool is granted does not include, so on a machine configured the way
  `CLAUDE.md` says the answer is `EPERM` and the cost is *unknown*, with the
  sysctl named as what makes it measurable. A sysctl turned off during a run
  makes the figure unknown at the close. A lower bound when known: the kernel
  times the program, not the tracepoint or trampoline dispatch, nor the slower
  syscall path an attached `sys_enter` puts every task on; and it counts every
  run, including ones for other tasks the program filtered out.
- **The preload shim** (`preload`): the shim's own clock, published in the ring
  it shares with `omn`. One hooked call in **17** per thread (odd, so a thread
  alternating lock and unlock times both) is timed from the hook's entry to its
  return, less the real function it wraps -- the allocation, the lock, the
  driver call are the program's own work -- less the two clock reads' worth that
  timing adds. The ring's header grew per-class `spent` and `timed` counters
  and the clock's calibrated cost; the slots moved to 320 and the magic to
  `omn_rng2`, so a shim and an `omn` that disagree about the layout fall back to
  the pipe rather than misread each other. Through the pipe the shim times
  nothing: unknown. A `Prepared::inside()` hook hands the account to the runner
  before the attach; `ShimPrepared` implements it once for every preload source,
  the mutex and GPU ones included.
- **Unknown by mechanism**: `pmu:*` (a perf event's counter is switched on
  every context switch, and a sampling event interrupts, in the target's
  context), `af_packet`, `so_timestamping`, `vendor:*` other than NVML,
  `fs:proc:clear_refs` and `fs:sys:page_idle` (clearing referenced bits makes the
  target refault).
- **Everything else is `0`**: procfs, sysfs, cgroup files, netlink, NVML read
  from outside the target's context.

### Ground truth

`tests/budget.rs`:

- `what_the_shim_spends_inside_the_target_is_most_of_what_the_program_lost` --
  the bench's allocation workload (`async-malloc 1 1000000`), three bare runs and
  three under `mem.heap.alloc`, each program's own cpu. The account's call count
  is the two million heap calls the program made to within 1 %, and the time is
  bounded above by what the program lost plus the spread, and below by half.
  Measured here, debug shim: lost 513-642 ms, account 383-471 ms. **Release
  shim**, by hand: lost 64-65 ms, account 62-64 ms on two of three runs (42 ms on
  the first) -- within the spread, which is the brief's bar and a debug build's
  unoptimised hooks cannot meet: what is not in the account is the jump into the
  hook and its first check, a few nanoseconds in a release build and tens in a
  debug one.
- `what_an_ebpf_program_spends_inside_the_target_is_measured_or_said_to_be_unknown`
  -- finding 47's own case, `proc.syscall` in `count` on the syscall workload.
  **Here the kernel is not timing programs** (`kernel.bpf_stats_enabled` = 0,
  `BPF_ENABLE_STATS` refused with EPERM), and the test asserts that half: the
  field is null, the record names the sysctl and the command, the percentage is
  the tool's own, and a `--budget 1000%` run is neither held nor exceeded but
  "could not be checked whole". The measured half -- the kernel's run count at
  least two per call the workload made, and the run time a part of the cpu the
  program lost across three runs each way -- **skips loudly** with the sysctl's
  value, and needs a machine where someone set it.
- `a_reader_from_outside_costs_the_target_nothing_and_a_counter_inside_it_is_unknown`
  -- `mem.rss` records `0`; `cpu.cycles` records null with the context-switch
  reason.

What the lock storm shows (`contended 4 1000000`, release shim, by hand): the
program's cpu grows by about 2.4 s under `sched.mutex`, and the shim's account
is 0.37-0.46 s. The rest is not the shim's cycles but what they do to a
contended lock -- a longer path between unlock and the next lock changes who
wins the handoff and how often a thread sleeps. Honest as a lower bound, and the
observer effect the roadmap asks the bench to measure, not a number the shim
can give.

## 2. The bands follow the measurement

**The rule.** A backend's band is the smallest whose worst case covers the
larger of two numbers measured by the bench on its family's reference workload
(`docs/bench.md`, "The workloads"): the tool's cpu as a share of the target's
at the default cadence, and the **dilation** the target measured of itself. The
first is what a poll costs, the second what an in-kernel or in-process
collector costs; either alone understated one kind. A fallback is never
declared cheaper than a mechanism it is costlier than per event (a uprobe is
not cheaper than the shim; a kprobe is not cheaper than an fentry), so a budget
never degrades onto something dearer. A fixed per-run cost -- an fentry
trampoline rebuild, NVML's initialisation -- is not a rate and is not what a
band describes; it belongs in a caveat.

Moved, `cost:` keys only:

| entry | backend | was | now | measured |
|---|---|---|---|---|
| `proc.syscall` | `bpf:raw_tp`, `bpf:tp` | high | **extreme** | dilation +296 % |
| `sched.futex` | `bpf:raw_tp`, `bpf:tp` | med | **high** | dilation +5.8 % |
| `sched.mutex` | `preload` | med | **extreme** | +1043 % (pipe); about +600 % on the ring |
| `sched.mutex` | `bpf:usdt`, `bpf:uprobe` | high | **extreme** | 1-2 us a call, dearer than the shim |
| `io.fs` | `bpf:fentry`, `bpf:kprobe` | low, med | **high** | dilation +7.5 % |
| `io.fsync` | `bpf:fentry`, `bpf:kprobe` | low | **high** | dilation +6.5 % |
| `io.syscall` | `bpf:fentry` | low | **med** | dilation +3.8 % |
| `mem.wss` | `fs:proc:clear_refs` | low | **high** | tool 6.4 %, dilation +2.7 % |
| `mem.heap.alloc` | `preload`, `bpf:usdt`, `bpf:uprobe` | med, med, high | **extreme** | about +1000 % on the ring (6 ms to 71 ms, release) |
| `mem.heap.live`, `mem.heap.retained` | `preload` | low | **extreme** | the same shim |

Two of these sit awkwardly on the scale: `extreme` is documented as "over
10x" (`catalog::Cost`, stream P's file) and the only band past 20 %; +296 %
and +600 % are in neither. `main.rs`'s text now reads "over 20 %, with no bound
above", and the doc comment and `omn-mcp`'s copy of the band text are filed.

Not moved, and next by the same rule (the collectors' owners may prefer a
slower default cadence to a higher band): `gpu.power` NVML 21.1 % and
`gpu.freq` 9.3 % at 100 ms, `power.cstate` 8.1 %, `mem.numa` 4.6 %, the
`smaps_rollup` readers 3.2-3.4 % on a 256 MiB target, `gpu.engine_busy` 3.8 %,
`gpu.mem` 1.4 %, `power.freq` 1.2 %, `net.tcp.rtt` 3.2 %, `io.latency` 3.7 %,
`io.size` 6.2 %, `io.queue` 2.8 %. And every `free` counter at 0.15-0.4 %,
which is the fixed cost of a 100 ms workload, not a rate. `io.syscall`
`bpf:raw_tp`/`bpf:tp` stay `med` unmeasured; they share `proc.syscall`'s
mechanism and pay it on every syscall of every task.

## 3. Losses per ring

`Collector::losses_by_ring() -> Vec<RingLoss>` (default empty),
`collect::RingLoss { key: KeyValue, losses: Losses }`, `Collection::by_ring`
and `Results::by_ring`. In the emit layer, `Shortfalls` places a ring's losses
on the series keyed by the ring's key, weighed against that key's own events,
when every loss the run counted belongs to a ring and the series are keyed by
the rings' kind; otherwise every series carries the run's, as before.
`rate`, `summary`, `hist` and `topk` read it per series; `stack` stays run-wide.

- Unit: `emit::tests::a_ring_s_losses_land_on_the_series_of_the_key_it_belongs_to`,
  `a_loss_no_ring_owns_is_the_run_s_on_every_series`.
- Scenario: `tests/runner.rs`,
  `a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed` --
  a busy task and a light one in one profile at 50 kHz. The rings do overflow
  here (3117 samples dropped in one run), but **`cpu.profile` does not report
  them per ring**: `src/collect/cpu/profile.rs` drains every task's ring through
  one callback that does not say whose record it is (`src/perf.rs`). Both are
  stream P's; the request is filed with the signatures. Until then the scenario
  skips loudly with the drop count.

**For `tests/cross_compute.rs` (J's)**: once the profile reports per ring,
`a_lossy_profile_marks_its_row_and_leaves_the_traced_rows_alone` can weigh the
decomposed task's own ring's drops rather than every sampled task's room, and
the series of the other tasks carry no mark.

## 4. What the shim stream holds is bounded

`mem::heap::HELD_RECORDS` = 2^19 records per stream for the sources that keep
an event per record (`mem.heap.alloc`, `sched.mutex`, `gpu.api`); past it each
record read is dropped and counted, with the reason "the shim reported more ...
than this run holds". The two that keep a total (`mem.heap.live`,
`mem.heap.retained`) read on: what they hold is the program's live set.

Ground truth, `tests/budget.rs`,
`a_lock_storm_is_read_bounded_and_every_acquisition_is_accounted_for`: four
threads, a million round trips each, the workload's own counter says 4 000 000.
Measured: **524 288 seen + 3 475 712 dropped and counted = 4 000 000 exactly**,
and `omn`'s high-water mark 1106 MiB (read from `/proc/<pid>/status` while it
ran), against 3.9 GiB for 1.9 million events before. An event of this class is
about two kilobytes in the event model; the bound is about a gigabyte of them.

## 5. The filed requests

| request | status |
|---|---|
| a counted window ends at its duration (`2947`, `6181` b) | **landed**: the drive loop wakes at the run's deadline. `tests/runner.rs`, `a_counted_window_ends_at_its_duration_and_not_at_the_next_read` (300 ms observed in 300-400 ms, was about a second) |
| `omn list --format json` carries the sweep (M) | **had landed** in the twelfth wave (`Listed`); its requirements now come from `resolve::probe_all` itself |
| `main.rs` calls `Cost::worst_case_percent` | **landed** |
| the filter line says who applied the predicate (W) | **landed** in `render.rs`: "in the program", "applied by the collector, after delivery", "applied after reading", from `Placed`'s reason, not its `Display`. `tests/runner.rs`, `the_filter_line_says_who_applied_the_predicate`. **The coordinator can now delete the `no;` prefix in `impl Display for Placed`** |
| `topk:N@<attr>` (`2755`, `3883`, `2600`) | **landed**: `Emit::TopK { k, by: Option<AttrName> }` (interned so `Emit` stays `Copy`), parse `topk`, `topk:N`, `topk:N@attr`, `topk:@attr`, round-tripped. Rows are `<attr>=<value>`. `tests/aggregation.rs`, `a_ranking_over_an_attribute_ranks_its_values` (two rows for a program making exactly N mallocs and N frees, `op=alloc` ≥ N events and 64 N bytes) |
| `count` can rank (`2467`, `6181` a) | **landed**: `ModeKind::Count` accepts `topk`. `a_counted_window_ranks_its_series` |
| `Results.overhead` and `served_by` (`2316`) | **landed**, both passed through from the collection. The `cost:` line is still printed by `main.rs` from `cost.json`'s record, which is the same number |
| a note is not a degradation (`3892`) | **the channel landed**: `Collector::facts()` → `Collection::facts` → `Results::notes`, rendered `note:` beside `degraded:` in text and pprof comments. No collector uses it yet: `net.softirq` is stream N's (filed) |
| a rate over the counter's own enabled time (`168`) | **landed**: a series whose every event carries `enabled_ns` is a rate over their sum, `SeriesRate::counted_window` says so and the text reads "over X the counter was enabled". For a per-task counter that is per second of the task's cpu, not of the wall -- a decision, below |
| `Results.period_ns` for pprof (`2127`) | **landed**: set for a sampling mode, written as pprof's `period` with `period_type` cpu/nanoseconds when every profile in the file shares one rate. `tests/encoders.rs` checks 1 003 009 ns at 997 Hz through the independent decoder |
| `HistScale` serialised, `hist:linear:0` refused at parse (`314`, `323`) | **landed**: one type, `emit::BucketLayout` is `request::HistScale` re-exported, serialised as `log2` / `linear:W`; width zero refused where the request is written |
| a multi-pid request refused by a collector that takes one (`250`) | **landed** without touching any collector: `Target::first_pid()` remembers when it narrowed several processes to one, and the runner asks after preparing and attaching each collector, refusing by name (exit 3) before the program is released -- also on a late attach and a degradation. The collectors that take the list (`io/vfs`, `io/block`, `net/request`, `net/tcp_rtt`, `runtime/usdt`, `mem/ibs`, `sched/mutex_usdt`) never call it and are unaffected. `tests/runner.rs`, `several_processes_asked_of_a_collector_that_observes_one_are_refused_by_name` |
| `open-requests 1845` (in-target cpu on `Overhead`) | **landed** as item 1 |

Not reached, and not in the brief's list: `973` (a `gpu` line in the text
environment dump) and `1040` (`emitted_fields`), both still open.

## 6. The tally's rows in these files

| # | test | fix |
|---|---|---|
| 11 | `collect::tests::what_a_run_reports_spending_is_the_cpu_it_actually_burned` | driven to 200 ms of spun cpu, with a 10 s wall cap only against a hang |
| 12 | `collect::tests::weighing_a_run_costs_a_fraction_of_the_run` | `measurement_ns` is now the weighing thread's cpu (`CLOCK_THREAD_CPUTIME_ID`), in the tool, not only in the test: a preempted weighing did not cost the wait |
| 13 | `budget::what_the_tool_says_it_spent_moves_with_how_often_it_was_asked_to_read` | weighed by the reads each run made (`events_seen`), with a named floor of a microsecond a read, and a loud skip when the machine did not let the fast run make five times the reads |
| 14 | `budget::the_extra_cpu_a_faster_poll_costs_is_the_extra_cpu_the_shell_observes` | a 2 s window, so the polling dominates the two runs' startups |
| 15 | `triage::a_detailed_layer_over_its_budget_is_let_go_and_the_escalation_goes_on` | a 20 ms hold, a hundred of them: the run queue's wait after each wakeup is a few percent of a cycle, not the half that kept the lock layer from being named |
| 27 | `encoders::a_known_allocation_lands_on_the_chrome_trace_at_the_time_it_was_made` | **had landed** (85c5a7d, the step found in the series at its largest rise). The workload's own stamp cannot be compared on chrome's axis, which is relative to the first event and carries no absolute origin |

And one not in the tally, found while fixing #11:
`collect::tests::an_escalated_layer_over_its_budget_is_let_go_and_the_run_goes_on`
failed 2 runs in 3 in `cargo test --lib collect::`. Its meter was
`/proc/<tid>/stat`, which for a thread id is **the whole process and every
child it reaped** -- so the forked children of other tests (#11's among them,
now spinning its full 200 ms) kept the escalated layer under its ceiling. A
`--tid` target's meter is now the thread's own account
(`/proc/<tid>/task/<tid>/stat`, `Meter::Threads`), which fixes the test and a
real `--tid` run's denominator alike.

---

## Checks

One `make check` per the rule, which stopped at its first red binary, then one
`cargo test --workspace --no-fail-fast` over the whole branch, then `cargo fmt
--check` and `cargo clippy --all-targets -- -D warnings`, both clean. What was
red, and why:

- `scenario_proc::the_combinations_these_sources_do_not_have_are_refused_with_the_reason`
  and `scenario_net::the_pairs_these_entries_imply_and_the_matrix_refuses_are_the_three_recorded`:
  **expected**, both assert that `count` cannot rank, which this branch changes
  on purpose (`open-requests 2467`). The edits are filed for S and N.
- `runner::the_filter_line_says_who_applied_the_predicate`: this stream's own,
  it found the request's filter line before the placement's; fixed.
- `scenario_mem::both_huge_page_backends_read_the_same_promotion_counter`: the
  tally's #21 (asserts what the machine's huge pages were doing); fails alone
  too, and is stream N's.
- `scenario_cpu::a_counted_window_is_the_same_number_in_every_format_that_carries_it`
  (1 026 137 289 instructions for 1 000 000 000, 2.6 %, the tally's #9 shape):
  passed alone at once.
- In the earlier `make check`: `bench_harness::the_io_workload_reads_from_a_cold_cache_without_root`
  (the harness's shared report name, filed) and
  `scenario_mem::a_walk_that_fits_in_l1_is_never_timed_and_one_that_does_not_waits_on_memory`
  (IBS sampling, stream N's); `perf::tests::closing_a_group_gives_its_descriptors_back`
  in another run, already in the tally's notes.

## Index rows for the sessions added

None: this stream added no session.

## For `docs/DECISIONS.md`

1. **The cost of a measurement includes what the collector spends inside the
   target**, and a mechanism that runs there without an account is *unknown*,
   never zero. The percentage is over the work the target did of its own
   (`observed − in-target`), which is the dilation it would measure; a ceiling
   over an unknown part is "not checked whole", not held.
2. **eBPF run time comes from the kernel's own `bpf_prog_info` counters**, read
   through `/proc/self/fdinfo` for the programs a request loaded, and only while
   the kernel is timing programs. This tool does not ask for `CAP_SYS_ADMIN` to
   turn that on: where the sysctl is off, the cost is unknown and the record
   says how to make it known.
3. **The shim publishes its own time**, sampled one call in 17, less the real
   function and the timing's own clock reads, in the ring it shares with `omn`.
4. **The band rule** in section 2: the larger of the tool's share and the
   target's dilation on the family's reference workload; a fallback is never
   declared cheaper than what it falls back from per event.
5. **A counted rate is over the counter's enabled time**, which for a per-task
   counter is the task's cpu, not the wall.
6. **A collector that observes one process refuses a request naming several**,
   decided at the runner by whether preparing or attaching it narrowed the
   target, rather than by a list of collectors.
7. **What a per-record shim stream holds is bounded** at 2^19 records a run;
   past it the reader drops and counts.

## For the coordinator

- `docs/CATALOG.md` §12's mode × emit matrix: `count` now accepts `topk`.
- `docs/CATALOG.md` / `catalog::Cost`: `extreme` is the only band past 20 %, and two
  measured backends sit between 20 % and 10x.
- The `no;` prefix in `impl Display for Placed` (`src/resolve.rs`) can go.
- `docs/bench.md`'s heap and mutex rows predate the ring; the next bench run
  replaces them and can read `in_target_cpu_ns` beside the dilation column.
- **The scratchpad directory is shared by every stream agent of one session.**
  A log this stream wrote there was overwritten by another stream's run of the
  same name mid-wave, and a `make check` result was nearly read from it. Each
  stream should write under a subdirectory of its own.

## Filed for other streams

In `docs/open-findings.md`, "Needs from another stream — the thirteenth wave,
stream R": per-ring drops in `cpu.profile` and `src/perf.rs` (P); the topk
attribute check in `src/validate.rs` (P); `Cost::Extreme`'s documentation (P)
and `omn-mcp`'s band text (J); the re-export that would let the
`BPF_ENABLE_STATS` call live in `src/bpf/syscall.rs` (S); `Collector::facts`
for `net.softirq`'s kernel count (N); and the harness's report name, which two
parallel tests of one binary share (`examples/bench/harness.rs`, nobody's).
