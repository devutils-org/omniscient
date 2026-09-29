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

---

# The thirteenth wave — stream S (the scheduler and the processes, per cgroup)

Stream S's deliverable. The `sched` and `proc` programs serve the cgroup their
entries declare, every one of them compares a thread for `--tid`,
`sched.switches -m count` counts, and what the two families carried wrong is
fixed or written down below, each checked against the code first.

| Files | `src/bpf/{insn,mod,runqlat,offcpu,migrate,wakeup,sched_lock,sched_futex,sched_irq,proc_events,proc_syscall}.rs`, `src/bpf/proc_renames.rs` (new); `src/collect/sched/{mod,runqlat,offcpu,migrations,wakeup,lock,futex,irq}.rs`, `src/collect/proc/{events,syscall}.rs`; `catalog/{sched,proc}.yaml`; `tests/{scenario_sched,scenario_proc,family_sched}.rs`; `examples/sched-cgroup.yml` (new), `examples/sched-irq-against-procfs.yml` |
|---|---|

## 1. A program narrowed to a cgroup (`open-requests 4328`, `4168`)

### What landed

`src/bpf/insn.rs` has one comparison every program now makes, instead of
eleven that each compared the thread group:

```rust
pub enum Whose { Any, Process(u32), Thread(u32), Cgroup(Cgroup) }
pub struct Cgroup { pub id: u64, pub level: u32 }        // Cgroup::resolve(path)
pub fn push_task_filter(program, &TaskFields, Whose, Subject, scratch, skip);
pub fn push_current_filter(program, Whose, scratch, skip);
```

**The mechanism, and why.** The two helpers the fifth wave offered were
`bpf_get_current_cgroup_id` (80) against one id, and
`bpf_current_task_under_cgroup` (37) over a `CGROUP_ARRAY`. The first answers
only for the cgroup a task is directly in, and a service's or a session's tasks
are in cgroups *below* the path a reader names: `--cgroup user.slice` would
have kept nothing. The second answers the ancestor question but only for the
*current* task, and half of these programs judge a task that is not current --
`sched_switch`'s outgoing and incoming tasks, `sched_waking`'s wakee, the task
a signal is for, the task being migrated. So the comparison is the kernel's own
`cgroup_is_descendant`, by level: a cgroup at level `L` contains a task when
the task's default-hierarchy cgroup is at least `L` deep and its ancestor at
`L` has the cgroup's id. For the current task that is one helper call,
`bpf_get_current_ancestor_cgroup_id(L)` (123); for any other task it is
`task->cgroups->dfl_cgrp`, its `level`, `ancestors[L]` and that ancestor's
`kn->id`, read with BTF offsets (6.1 and later; before 6.1 the ids themselves
are `ancestor_ids[L]`, which the walk also reads). No map, and the same cost at
any depth. The id is the directory's inode, which is `kn->id`; the level is
the path's depth below `/sys/fs/cgroup`.

**Served**, through the in-kernel comparison, and in the same commit the
`cgroup` key of each entry is now true: `sched.wakeup` (either end: the wakee
through the walk, the waker through the helper, never in an interrupt),
`sched.runqlat`, `sched.offcpu`, `sched.migrations`, `sched.lock`,
`sched.futex`, `proc.exec`, `proc.exit`, `proc.signals` (either end) and
`proc.syscall`. `src/collect/sched/mod.rs::whose` is the one target match the
sched collectors share.

**Not served, and taken out**: `container`. Nothing in the build resolves a
container to its cgroup (`Target::Container` reaches every collector
unresolved), so the key is gone from `sched.wakeup`, `.runqlat`, `.offcpu`,
`.migrations`, `.switches` and `proc.syscall`, `.exec`, `.exit`; a resolver
is filed below, and the key comes back with it. `sched.switches` loses
`cgroup` too: see section 2.

**One case it does not serve**: a cgroup namespace. The level is read from
the path below the mount, and inside a namespace the mount's root is the
namespace's, so the level is short by the namespace's depth and the comparison
asks the wrong ancestor -- the kernel keeps nothing a process can read to
correct it. Said in `Cgroup::resolve`'s doc; nothing in this build detects it.

**On older kernels**: helper 123 in a raw tracepoint program and the
`ancestor_ids` shape are written and not exercised here (7.2 only); the 5.15
and 6.6 cells are stream N's and the coordinator's sweep.

### The ground truth

`scenario_sched::a_cgroup_target_sees_the_workload_inside_it_and_none_of_the_one_beside_it`
and `scenario_proc::a_cgroup_target_sees_the_forks_signals_and_calls_inside_it_and_none_beside_it`:
a cgroup made under the test's own (with a child, where the workload goes, so a
run on the parent must follow the ancestry), and the same workload beside it.
Each copy logs what it does on `CLOCK_MONOTONIC`; what it did after the window
opened is the floor, what it did after `omn` was spawned the ceiling, and the
workload stops two seconds before the run can, so nothing falls in the gap
after the last drain (section 6). Measured:

```
sched.offcpu --cgroup: 1620 sleeping blocks kept for 1602 to 1692 the kernel counted
sched.wakeup --cgroup: 1619 wakeups of the task inside for 1602 blocks; 1386 of the
  task beside it by the task inside, for 1693 signals
proc.signals --cgroup: 690 to itself for 689 to 717 sent; 690 to the process beside it,
  kept for their sender, for 689 to 717
```

The block reference is the kernel's own: each sleep carries the thread's
`ru_nvcsw` delta, because a sleep whose timer fires while the thread is
preempted does not block, and a loaded machine does that to a few percent.
Nothing of the copy beside the cgroup is kept by any of the ten, and every
wakeup of it that is kept was made by the copy inside.

## 2. `sched.switches -m count`

The `pmu:sw` factory hands a counted request to
`crate::collect::cpu::counters::Prepare::new(&counters::SCHED_SWITCHES, Mode::Count)`,
as stream W described it (P's file, used and not changed): one inherited
software counter per thread, read at the end.
`a_declared_mode_the_build_refuses_says_what_is_missing` is now
`a_counted_switch_total_is_the_sum_of_what_each_thread_counted`, on the switch
fixture whose four threads each sleep a hundred times and read their own
`/proc/thread-self/status` (and now the main thread too): the total is at
least the four threads' sum and at most everyone's plus the edges. No `sched`
mode the catalog declares is refused any longer, so the refusal test has
nothing left to move to. `SCHED_SWITCHES`'s `#[allow(dead_code)]` can go (P's).

**`cgroup` and `container` are out of `sched.switches`' targets.** No backend
of the entry has a collector for either: `pmu:sw` follows threads or cpus, and
the `bpf:raw_tp` line has no collector at all. Serving a cgroup here means a
`sched_switch` program, which would be first in the chain for *every* target
and would change what five waves of switch scenarios measure (it would also
tell voluntary from involuntary, which the caveat says only it can). That is a
decision, below, rather than a line.

## 3. What `sched` and `proc` carried wrong

Each checked against the code first; all five were open.

- **`comm` is the name at row creation** (`open-requests 1392`, finding 20).
  `src/bpf/proc_renames.rs`: `sched_process_exec` attached beside
  `sched.runqlat`, `.offcpu` and `.migrations`, narrowed to the same target,
  writing each exec'd task's new name into an LRU map the collector drains
  after its rows. The last drain's names are applied once more, because a
  drain retires its row map and waits a grace period before reading it, and
  an exec inside that grace period is taken by this drain while the rows made
  before it are in the next one's map -- which is what a first version
  missed. `a_launched_programs_rows_carry_the_name_it_exec_d_into`: a launched
  `sh -c 'sleep 0.02; sleep 0.02'` has rows named `sh` and `sleep` and none
  `omn`. Optional: a refused exec program is a note, and the rows keep the
  name they were made with.
- **`sched.offcpu --emit hist` needs a bucket in its key** (`624`) **and
  `TOTAL_OF_ATTR`** (`2641`). The bucket landed: the totals key is
  `(tid, tgid, state, stack, log2 bucket)`, and the value is `block_ns`, the
  mean block of a row, weighed by `blocks`. Every block of a row is in its
  bucket, so the mean is, so `hist` and `summary` are of blocks; `rate`,
  `topk` and `stack` add `block_ns × blocks`, which is `total_ns`, exactly as
  before. `TOTAL_OF_ATTR` is therefore not carried: it marks a row whose
  measurements are gone, and these are not -- `emit.rs`'s own doc says the
  attribute goes away once the bucketing moves into the kernel.
  `an_off_cpu_histogram_is_of_blocks_and_finds_both_modes_of_a_bimodal_sleeper`:
  forty 3 ms sleeps and twenty 40 ms ones come back as 40 blocks in
  `[2^21, 2^22)` and 20 in `[2^25, 2^26)`, 60 in the series. Decompositions
  name `total_ns` explicitly and are unaffected.
- **The soft half of `sched.irq`** (`4304`). `softirq_entry` / `softirq_exit`
  are attached and required; a row says `kind: hard | soft`; a softirq's `irq`
  is its vector and its `name` the vector's (`NET_RX`, `TIMER`, ...); a hard
  handler that lands inside a softirq is taken out of the softirq's length
  through a per-cpu running total, so the halves add up. Summary: "hard and
  soft" again. `every_interrupt_this_run_timed_is_one_the_kernel_also_counted`
  checks the soft half against `/proc/softirqs` the way it checks the hard
  half against `/proc/interrupts`: `softirq SCHED: 2603 runs timed against 2996
  the kernel counted` over a bracket that contains the run.
- **A thread target that compares the thread** (`4927`). `Whose::Thread`
  compares `task->pid` (or the low half of `bpf_get_current_pid_tgid`) in every
  program. `a_thread_target_keeps_that_thread_and_not_the_one_beside_it`:
  `sched.offcpu --tid: 1357 blocks kept for 1347 to 1449 the kernel counted`,
  and nothing of the process's other thread in any of six sources.
  `tests/cross_io.rs`'s pinned defect now fails as its own message says it
  should; filed below.
- **`proc.syscall`'s program counts what its comparison skipped** (stream W,
  twelfth wave). `SKIPPED_BY_PREDICATE`, slot 2 of the drop counter, counted at
  the syscall comparison's skip; `SyscallProgram::skipped_by_predicate()`; the
  note says "the program skipped N calls to other syscalls without timing
  them", and `a_predicate_the_program_carries_is_placed_in_the_kernel_...`
  asserts it (at least a hundred over a Python start-up).

- **Found and fixed: `sched.runqlat --cpu`.** The entry declares `cpu:
  native` and the collector refused every cpu target. A cpu target is that
  cpu's queue: the program watches every task and the rows are kept for the
  cpu each wait was served on, which is in the kernel's key, with a note
  counting what was set aside. `family_sched::a_cpu_target_is_that_cpus_run_queue`:
  three tasks pinned to one cpu are each counted for what their own
  `run_delay` says they waited there, and no row of another cpu is kept.

## 4. The catalog's words

- `sched.wakeup`'s interrupt caveat as stream D wrote it: **already on
  `main`**, word for word.
- The observer effect on `sched.lock`, `.offcpu` and `.runqlat` (finding 47):
  **already on `main`** ("the bench measured the contended workload 21 % / 11 %
  / 9 % faster under it").
- `sched_process_exit(task_struct *)` on 6.6 (stream V's list): the program
  reads both shapes and the requirement is
  `raw_tracepoint:sched_process_exit(task_struct *, bool)|(task_struct *)`. On
  the older shape `group_dead` is `signal->live == 0`, read where `do_exit` has
  already decremented it, and the run says two threads leaving at once can
  both carry it. Not exercised here (7.2 passes `group_dead`).
- `sched.irq`'s summary, requirements, `kind` filter field and a caveat on
  the two halves; `sched.offcpu`'s aggregation caveat names the bucket and the
  value.

## 5. The tally's rows in these files

| # | Test | What changed |
|---|---|---|
| 1 | `one_known_number_of_round_trips_is_found_by_four_sources` | wakeups are the pair's two heaviest wakee series, not the run's total (the switches' floor and ceiling were already in) |
| 2 | `a_workload_that_is_half_blocked_measures_as_half_blocked` | its leftover: the blocked half is `blocked_ns`, the total less the queue wait |
| 3 | `an_overflowing_wakeup_ring_says_what_it_could_not_hold` | the no-loss branch asserts the floor only |
| 5 | `a_task_moved_between_two_cpus_is_counted_once_per_move` | the task is moved until the run ends, each move stamped, and the moves inside the reported window are the floor: 60 inside, 60 counted |
| 6 | `tasks_sharing_one_cpu_wait_the_share_the_scheduler_owes_them` | each task's waits against its own `/proc/<tid>/schedstat` `run_delay` over a bracket around the run: no more than the kernel's delta, no less than it less the part of the bracket the run was not attached for; 674 ms counted of 797 ms over 1.03 s of a 1.19 s bracket |
| 7 | `what_the_signal_ring_could_not_hold_is_counted_and_closes_against_what_was_sent` | one storm per process, back to back while the run lasts, and the storms wholly inside the window checked; `late_ticks == 0` dropped (a late drain is also why the ring filled) |

`ATTACH_MARGIN` had four more users in `scenario_proc.rs`, and three of them
failed under load in this stream's runs the same way #7 does. All five now use
`while_running`, which repeats the action until the run ends and stamps each,
with `Acted::inside` bounding by the window *and* by the earliest the run can
end. And a copy of `/bin/sh` just written is retried on `ETXTBSY`, which a
process another test forked holds for a moment.

## 6. Found on the way: a window closes after its collector's last read

`collect::run` takes one `closed_ns` after every collector has been stopped.
For a drained source the last read is in its own `stop`, which is earlier: a
block that ended between `sched.offcpu`'s final drain and the moment the last
of six collectors stopped is inside the reported window and in no row. On
this machine that was 6 blocks of 930 with `sched.offcpu` alone and 71 with
five programs stopping after it. The scenarios here act only until the
earliest the run can end, so they do not depend on it; filed for R.

## Needs from another stream

In `docs/open-findings.md` under "the thirteenth wave, stream S".

## The suite

One `make check` before the commit, at a load of 20 to 27 with the other
streams' suites beside it: red at
`collect::tests::the_runner_closes_the_window` (stream R's, "came 9910729 ns
early"), which passed alone at once. The one `cargo test --workspace
--no-fail-fast` after it: **1918 passed, 4 failed**, and none of the four is a
defect of this stream's:

| Test | Why |
|---|---|
| `cli::the_session_indexes_hold_exactly_the_sessions_on_disk` | the index row for `sched-cgroup.yml`, below |
| `cross_io::a_thread_target_on_the_scheduler_half_keeps_that_threads_events` | the defect it pins is fixed; filed |
| `collect::cpu::profile::tests::a_profile_of_the_machine_finds_this_thread_for_the_cpu_it_burned` | stream P's; passed alone at once |
| `scenario_net::a_connection_is_joined_from_its_handshake_to_its_close_by_the_cookie_alone` | the tally's #17; passed alone at once |

The second commit (`sched.runqlat --cpu`) had its own `make check`: red at
two tests of the tally, `perf::tests::closing_a_group_gives_its_descriptors_back`
and `collect::tests::weighing_a_run_costs_a_fraction_of_the_run` (#12), both
passing alone at once; and its `--no-fail-fast`: **1919 passed, 4 failed** --
the index row and `cross_io`'s pinned defect again, and
`join_engine::a_half_blocked_task_decomposes_as_the_kernel_says_and_its_releaser_heads_the_chains`
(529 of 600 wakeups folded in the top chain, nothing counted lost) and
`scenario_io::a_sequential_read_is_read_ahead_of_and_a_random_one_is_not`, each
passing alone at once, the first twice; both filed for the tally.

`tests/scenario_sched.rs`, `family_sched.rs`, `scenario_proc.rs`,
`family_proc.rs` and `raw_tracepoints.rs` passed whole, three times each for
the two scenario files, under the same load. `cargo clippy --all-targets -D
warnings` and `cargo fmt --check` are clean.

`scenario_proc::the_combinations_these_sources_do_not_have_are_refused_with_the_reason`
now opens with `proc.syscall -m count -e hist` rather than `proc.signals -m
count -e topk`, which stream R's merge makes a ranking: the refusal it asserts
holds on both sides of that merge.

## Index rows

For `examples/README.md` and `GUIDE.md`, in the `sched` table:

```
| `sched-cgroup.yml` | A cgroup and everything below it: its blocked time, its run queue waits, and who woke it. |
```

## For `DECISIONS.md`

- **`--cgroup` means the cgroup and every cgroup below it, compared in the
  kernel**, by level against the task's default-hierarchy ancestry: the
  helper for the current task, a BTF walk for any other. Not
  `bpf_get_current_cgroup_id`, which misses descendants, and not a
  `CGROUP_ARRAY`, which answers for the current task only.
- **A thread target compares the thread, a process target the thread group**,
  in every program; `Target::first_pid` is no longer what a program compares.
- **A kernel-bucketed source's value is the per-measurement mean, weighed by
  the count** (`sched.offcpu`'s `block_ns`), so `hist` and `summary` are of
  measurements and every sum is still exact.
- **Open**: whether `sched.switches` gets a `sched_switch` program that serves
  every target (and a cgroup, and tells voluntary from involuntary), or stays
  on `pmu:sw` with no cgroup. Until decided the catalog declares what is
  served.
