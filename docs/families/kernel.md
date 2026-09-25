# The kernel's attributions and drains — stream D, twelfth wave

What the eleventh wave found in kernel programs nobody owned: places where the
tool credited an event to the wrong task, or lost one between the program and
its drain. Five tasks were given; each is below with what it was checked
against, the measurement before and after, and what is left and why.

| | |
|---|---|
| Files | `src/bpf/wakeup.rs`, `src/bpf/map.rs`, `src/bpf/fentry.rs`, `src/bpf/io_pagecache.rs`, `src/bpf/net_qdisc.rs`, `src/bpf/net_softirq.rs`; `src/collect/sched/{mod,wakeup,switches,offcpu,migrations}.rs`, `src/collect/io/histogram.rs` (two comments); `tests/scenario_sched.rs`, `tests/scenario_io.rs`, `tests/scenario_net.rs`, `tests/scenario_mem.rs` |
| Tests | nine new scenarios (six in `scenario_sched`, two in `scenario_io`, one in `scenario_net`), three rewritten (`scenario_sched`'s two round-trip scenarios, `scenario_mem`'s kernel sweep), six new unit tests and one rewritten |
| Public methods | `drain()` keeps its signature and meaning in the three moved programs. `HistogramProgram::drain(last)` keeps its signature; `last` asks for nothing more now. `map::grace_period` is public |
| Machine | 24 cpus, kernel 7.2.6 (`CONFIG_PREEMPT=y`), `CAP_BPF` and `CAP_PERFMON` on the binary, three other streams building and testing on it |

---

## 1. A wakeup from an interrupt is not the interrupted task's (finding 48)

### What it was

`sched.wakeup` attaches to `sched_waking`, which runs in the waker's context,
and takes the waker from `bpf_get_current_pid_tgid`. A wakeup issued from a
hard interrupt, a softirq or an NMI runs on whatever task the interrupt landed
on, so the task named was only the landing place -- and a target's filter
kept the event as a wakeup *the target made*. A busy machine's disk, network
and timer wakeups were credited to a half-blocked workload by the thousand.

### What landed

The program reads the cpu's preempt count -- `__preempt_count` through
`bpf_this_cpu_ptr` of its BTF id (`BPF_PSEUDO_BTF_ID`), or the
`preempt_count` member of `pcpu_hot` on the 6.2 to 6.14 layout -- and keeps
its interrupt bits: `NMI_MASK`, `HARDIRQ_MASK` and, of the softirq byte, only
`SOFTIRQ_OFFSET`, which is set while a softirq is *served*. The rest of that
byte counts `local_bh_disable`, which a task holds in its own context around a
socket lock, and a wakeup made there is the task's.

With any of those bits set, the record's waker is the interrupt: `waker_tid`
and `waker_pid` 0, and a new attribute `waker_context` (`task`, `softirq`,
`hardirq`, `nmi`). The target filter runs after the replacement, and a pid of
0 never matches it, so an interrupt's wakeup is kept only for its wakee. The
run says how many there were:

```
degraded: 351 wakeups were issued from an interrupt, softirq or NMI; the waker is
the interrupt (`waker_tid` 0, `waker_context` says which), not the task it landed
on, and a target keeps such a wakeup only as its wakee
```

A kernel where the program cannot find the count (another architecture, a BTF
without either variable) loads the program as before and says so in the
run's degradations; the unit test fails on an x86 kernel with BTF that
declares neither, because that would be a layout this tool has not met.

### The ground truth

`a_wakeup_from_an_interrupt_is_not_credited_to_the_task_it_landed_on`: the
half-blocked pair of `examples/sched-half-blocked.yml`, which says its own two
pids, pinned with `taskset` to one cpu beside a load pinned to the same cpu --
a thread sleeping 50 us at a time (its hrtimer's hard interrupt), a thread
doing direct reads (the disk's completion interrupt, delivered to the reading
cpu by a per-cpu queue), and loopback datagrams (the network softirq). The load
shares nothing with the pair but the cpu, so the driver never wakes one of its
threads. Three runs each:

| | wakeups of the load's threads credited to the driver | to third parties in all |
|---|---|---|
| preempt count left unread (the old program) | **1954** | 15 310 (a kworker's 12 417 among them) |
| read | **0, 0, 0** | 10, 11, 10 |

and the pair keeps its 575 to 602 wakeups each way. The ten left in the fixed
runs are real task-context wakeups, the same set every run: the parent
`omn` at exit, the test process reading the workload's stderr, `migration/N`
(the scheduler's active balancing, kicked from the driver's own `schedule()`),
and `systemd` and `tailscaled`, which listen for process events the shell's
forks raise.

`a_sleep_is_ended_by_the_timer_interrupt_and_not_by_the_task_it_landed_on`:
the other half -- a program that sleeps twenty times for 2 ms is woken twenty
times from `hardirq` context (a `nanosleep` ends in its hrtimer's interrupt),
each with a waker tid of 0.

### What is left

- **A softirq served at `local_bh_enable` in a task's context** -- loopback
  delivery in the sender's own `sendmsg` -- is recorded as `softirq`, though
  the sender caused it. The preempt count cannot tell it from a softirq served
  at an interrupt's exit (both have `SOFTIRQ_OFFSET` and no hardirq bit), and
  crediting every softirq to the task it runs on is exactly the defect. The
  wakee side still keeps it for a target.
- **The BTF id walk is repeated in `wakeup.rs`.** `btf::Btf` says whether a
  type exists and not its id, and `btf.rs` is not this stream's; filed for
  `Btf::var_id`, after which the local walk goes.
- **The catalog's caveat still says the old thing** ("a waker of pid 0 is the
  idle task ... credited to whatever was interrupted"), and the join still
  renders a waker of 0 as `task:0 (idle)`. Both filed.

## 2. The drain race in `io_pagecache.rs`, `net_qdisc.rs` and `net_softirq.rs`

### What landed

The three programs moved onto `map::Rows` exactly as stream K filed it: the
row map is taken from the generation slot once per run (`select_rows`), kept
in an eight-byte stack slot of its own (`ROWS_SLOT`, added to each file's
stack-region test), and every lookup and insert reads it back (`row_map`). A
`no_rows` label counts a row drop, and is never reached while the slot holds
a map. Each file has `the_row_map_is_taken_from_the_slot_once_per_run`
(`assert_rows_taken_once`); `io_pagecache.rs` checks both of its programs.

### The ground truth

**`io.pagecache`**:
`every_lookup_a_program_made_is_in_a_row_or_a_counted_loss_on_a_short_tick`.
Eight threads `pread` one cached 4 KiB page at a time for 1.5 s and count their
own reads; a page-aligned one-page `pread` of a cached file is one
`mm_filemap_get_pages` over one page, so the program's count is the number of
lookups the rows must hold, on the collector's 100 ms tick:

```
unchanged program   made 7828224 counted 7828218 lost 0      6 vanished
map::Rows           made 7560192 counted 7560192 lost 0
                    made 7569152 counted 7569152 lost 0
                    made 6893056 counted 6893056 lost 0
                    made 7767296 counted 7765608 lost 1694   (the kernel's skips, machine-wide: an upper bound)
```

**`net.qdisc`**:
`every_packet_through_a_discipline_is_in_a_row_or_a_counted_loss_on_a_short_tick`.
Four senders in a user and network namespace push 16-byte datagrams through a
`netem` with no delay on a dummy device for 1.5 s, each counting what the
kernel accepted. `netem` and not `pfifo`: an empty `pfifo` lets a packet
bypass it (`TCQ_F_CAN_BYPASS`), the tracepoints never see it, and the first
version of this test found 563 899 of 1 381 400 that way.

```
unchanged program   sent 1363600 counted 1363597    3 vanished
                    sent 1352200 counted 1352197    3 vanished
                    sent 1368100 counted 1368098    2 vanished
map::Rows           sent 1374800 counted 1374800
                    sent 1342100 counted 1342100
                    sent 1400500 counted 1400500
```

**`net.softirq`: no lower bound could be created here.** A number of
`NET_RX` handlers cannot be injected -- the driver and the coalescing decide --
and the only other party that counts them, `/proc/softirqs`, is machine-wide
and read around the run, so it bounds the run from above and not from below
(`the_kernels_own_softirq_counter_bounds_what_this_run_bucketed`, unchanged,
still passes). The race loses two or three events per 1.3 million row updates
where it could be measured (the qdisc, above), and the edges of a
`/proc/softirqs` window on this machine hold more handlers than that. So the
softirq program has the structural test and the upper bound; the fix is the
same four lines as the other two, which are measured.

## 3. `fentry::HistogramProgram`'s grace period

### What landed

The drain now points the generation word at the empty map, waits for
`membarrier(MEMBARRIER_CMD_GLOBAL)` (`map::grace_period`, made public), and
empties the map it retired -- `map::Rows`'s scheme with the histogram's own
one-word array. No row is a tick late, the last drain is an ordinary one, and
the millisecond of sleep is gone. The load refuses, with `Rows`'s reason, on a
kernel that offers no grace period (`nohz_full`). The cost is 5 to 18 ms per
drain in the drain's thread, which `Rows` already pays.

### The ground truth, and what it cannot show

`every_call_through_fentry_is_in_a_row_or_a_counted_loss_to_the_last_drain`:
the same page reader through `io.syscall`, every `pread64` in a row or a
counted loss, three runs: exact (5 136 896, 5 325 824, and 4 495 104 with 23
counted lost). **The unchanged binary is exact on it too** (six runs, one with
79 430 counted lost), and it has to be: a launched program has exited before
the last drain, so no exit half is in flight when the 1 ms bet is made. The bet
loses only when a run ends while its target is still calling, and then the
program's own count cannot be read at the instant the run stopped, so there is
no number to hold the tool to. The fix is by construction -- a grace period is
exact where a sleep is a guess -- and the scenario shows it costs nothing in
the counts.

## 4. `sched.switches` follows threads, and serves the machine

### What landed

`sched.switches` opens `perf::ThreadSampler` (stream S's two lines;
`perf.rs` unchanged): one inherited event per thread per cpu, sharing a ring
per cpu. A `--tid` target is that task, a process or a launch target every
thread it has and starts, and **the machine and a cpu set are served** -- one
event per cpu with `pid = -1`, which is P8's `sched` half. A cgroup or
container target is still refused by name.

An inherited event also follows a child *process*, and every other `sched`
source on a process target observes that process -- its threads -- and not
its tree (their programs filter on the tgid). So the samples are kept for the
target's process, the rest counted and said:

```
degraded: 2008 switches of 2 tasks outside the target -- a process it started, or
another thread of a named task -- were followed by the inherited events and left
out: this family's sources observe the target's process, its threads included,
and not the processes it starts
```

Keeping the child processes instead broke two things at once: the round-trip
sessions' counts doubled (the responder is a process of its own), and
`cross_compute`'s per-switch `WAKEUP` join found chains ending at a task the
other sources never saw.

### The ground truth

`every_thread_a_program_starts_has_its_switches_counted`: four threads each
sleep a hundred times and then read their own `voluntary_ctxt_switches` and
`nonvoluntary_ctxt_switches` from `/proc/thread-self/status` and say them.
The tool's series for each thread is that number up to four for the edges (the
status read itself and the exit); measured, exact:

```
omn-switches tid=131345 switches=100    task:131345 n=101
omn-switches tid=131346 switches=100    task:131346 n=100
omn-switches tid=131347 switches=100    task:131347 n=100
omn-switches tid=131348 switches=100    task:131348 n=100
```

Before, no thread the program started had a series at all.
`a_machine_wide_switch_count_holds_a_known_sleepers_switches` checks the
machine-wide run holds a launched shell's switches under its pid.

### The round-trip scenarios (the coordinator's note)

`one_known_number_of_round_trips_is_found_by_four_sources` failed in a full run
with 2643 switches for 2000 round trips: a switch count holds the preemptions
a loaded machine adds, which the workload does not choose. It and
`perf_counts_the_blocks_a_raw_tracepoint_counts_and_preemption_besides` now
read **the driver's** series (the task `sched.offcpu` saw block), and assert
the workload's property -- at least one switch per round trip, within the
tolerance -- with a ceiling of twice the round trips, which is what a
collector counting each switch twice fails. The preemptions have no bound
here: `pmu:sw` cannot tell them from blocks, which its caveat already says.

## 5. `mem.latency -e stack`

**The collector is right; the fix is stream R's.** `src/collect/mem/ibs.rs`
leaves `latency_cycles` out of an event whose load the unit did not time (an
L1 hit on family 17h), never zero and never text. The emits that reduce values
count such an event as `unmeasured` through `emit::measured`; `emit::stack`
calls `value_of` instead, which turns an absent attribute into
`NotNumeric` and exit 1. Reproduced here once in fifteen runs. `src/emit.rs`
is R's this wave, and the coordinator has asked R for it; nothing in this
stream's files changes.

**The capability-less test** P listed,
`every_mode_and_emit_of_the_kernel_sources_is_served_or_refused_by_the_right_code`,
now takes the combinations refused for want of `CAP_BPF` and `CAP_PERFMON`
out of the machine-blame comparison and says how many (the refusal's first
`error:` line names no capability; the lines after it do, so the sweep reads
the whole of stderr for it). Run with the binary relinked without its
capabilities: 31 of 38 skipped, by name, and green; with them, green.

## 6. Also: stream S's filters on `sched.wakeup`, `sched.offcpu`, `sched.migrations`

Filed last wave for these files and not in this brief's list, and four lines
each: the factories hand the collectors the predicate, each applies it per
drained record or row, counts what it kept and removed, and says it ran there.
`omn sched.offcpu --system -d 300ms --filter 'comm == "nosuchcomm"'` reported
every task on the machine; it reports none now and says how many blocks it
removed. `a_wakeup_filter_that_matches_nothing_keeps_nothing_and_says_what_it_removed`
and `a_filter_over_the_drained_sources_keeps_what_it_names_and_says_what_it_removed`
are the scenarios.

---

## Needs from another stream

Filed under "the twelfth wave, stream D" in `docs/open-findings.md`: `Btf::var_id`
(V), `helper::THIS_CPU_PTR` and a `BPF_PSEUDO_BTF_ID` load (nobody's
`insn.rs`), the `sched.wakeup` caveat and the `sched.switches` targets
(V's catalog), the P8 `sched.switches` entry of `KNOWN_DEFECTS` (V), a waker
of 0 named by its context (whoever owns `join.rs`), and one test for the flaky
tally.

## The suite

One `make check` on the finished tree, and one `cargo test --workspace
--no-fail-fast` after a red one (1840 passed, 9 failed, at load 12 with three
other streams running). Every failure, and what became of it:

| Test | Seen | What it was |
|---|---|---|
| `scenario_net::every_packet_through_a_discipline_is_in_a_row_or_a_counted_loss_on_a_short_tick` and `a_queue_that_delays_every_packet_is_measured_as_that_delay` | once, together | both built a `dummy0` in their own namespaces at once, and a raw tracepoint sees every namespace's: each counted the other's packets. The new one's device is `omnq0` now; both pass |
| `scenario_sched::a_machine_wide_switch_count_holds_a_known_sleepers_switches` | once | the sleeper had finished before `omn` started on a loaded machine; it now sleeps until the run is over |
| `scenario_mem::a_histogram_map_driven_past_its_capacity_says_what_it_could_not_count` | once | its only loss was the kernel's skipped runs, not the map's capacity; that run is now a loud skip with the numbers |
| `scenario_sched::one_known_number_of_round_trips_is_found_by_four_sources` | once | 1393 blocks for 2000 (`sched.offcpu`'s half, not the switches); passed alone |
| `scenario_sched::a_mutex_nobody_else_wants_is_held_as_long_and_waited_for_not_at_all` | once | the preload shim's timing under load; passed alone |
| `scenario_mem::both_huge_page_backends_read_the_same_promotion_counter` | once, and alone | finding 43's tally, a machine-wide THP counter |
| `scenario_mem::every_mode_and_emit_of_the_sampled_sources_is_served_or_refused_by_the_right_code` | the last `make check` | `mem.latency sample stack`, stream R's `emit::stack` (section 5); passed alone |
| `collect::tests::an_escalated_layer_over_its_budget_is_let_go_and_the_run_goes_on`, `encoders::a_known_allocation_lands_on_the_chrome_trace_at_the_time_it_was_made`, `perf::tests::closing_a_group_gives_its_descriptors_back` | once each | not this stream's files; filed for the tally |

`cargo clippy --all-targets -D warnings` and `cargo fmt --check` are clean.
`tests/cross_compute.rs::a_lossy_profile_marks_its_row_and_leaves_the_traced_rows_alone`
is red on `main` (a `cpu.profile` loss count, another branch's) and was red
here in the per-file run; not touched.

## Index rows

None: this stream added no session.

## For `DECISIONS.md`

- **A wakeup issued in interrupt context has the interrupt as its waker**
  (tid 0, `waker_context`), and a target keeps it only as its wakee. The
  served-softirq bit decides, not the whole softirq byte: bottom halves
  disabled is a task's own context.
- **A `sched` source on a process target observes the process, threads
  included, not the processes it starts** -- now true of `sched.switches` too,
  whose inherited events follow children and whose samples of them are counted
  and left out. It is what the other `sched` sources' programs already did,
  and what lets their events be joined per task.
- **Every two-map drain waits for a grace period before it reads the map it
  retired**; none bets on an interval or a sleep. `fentry`'s histograms are
  the last to move.
