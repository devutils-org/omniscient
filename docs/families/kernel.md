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

---

# The fourteenth wave — stream K (the kernel programs)

The stock machine from the kernel side: what the CI's two runs on Linux 6.17
(`../omniscient-ci-logs/`, paranoid 4 and paranoid 2, no capabilities) showed
in the `sched`, `proc`, `io`, `net` and `mem` files, the read-ahead lookup the
coordinator filed, the requests the thirteenth wave left for S and N, and the
one open line of `DECISIONS.md`.

## 1. 6.17's prototypes

**`tcp_v4_connect`: the program had both shapes; the requirement and the test
did not.** The CI logs read the other way round from the filing: 6.17 declares
`tcp_v4_connect(sock *, sockaddr *, int)`, and it is 7.x (this machine) that
declares `sockaddr_unsized *`. `net_connect.rs` already accepted both. What
failed was `the_functions_the_five_sources_time_have_the_prototypes_they_read`,
which held the kernel to 7.x's shape. Now:

- **An `fentry:` requirement may carry its prototypes**, like a raw
  tracepoint's: `fentry:tcp_v4_connect(sock *, sockaddr_unsized *, int)|(sock *,
  sockaddr *, int)` in `catalog/net.yaml`, compared by `fentry::support` in the
  words `fentry::expect` uses, so a kernel whose prototype the program has
  never seen reads *missing* in `omn list` instead of failing at attach. A bare
  name still asks for existence only.
- `the_catalog_lists_every_shape_of_the_connect_call_the_program_reads` holds
  `CONNECT_SIGNATURES` and the catalog line to each other;
  `a_requirement_with_a_prototype_is_missing_when_the_kernel_declares_another`
  checks the probe against this kernel's own `vfs_read`.
- The prototype test accepts either shape of `tcp_v4_connect`, by the
  program's own constant.

**`mem_stall`: landed in the thirteenth wave** (N, `c32243f`): `RECLAIM_BEGIN`
lists `(gfp_t, int, mem_cgroup *)` and 6.17's `(int, gfp_t)`, the catalog line
lists both, and the CI's second run (which contains `c32243f`) passed
`the_running_kernel_declares_the_tracepoints_these_programs_attach_to` on
6.17. Nothing to do.

Ground truth: this machine (7.2) unchanged -- `net.tcp.connect` still usable
through `bpf:fentry`, every `fentry` and `net_connect` test green -- and, for
6.17, the CI's next run.

## 2. The read-ahead lookup: counted, not silent

`scenario_io::a_sequential_read_is_read_ahead_of_and_a_random_one_is_not` now
prints the run's `losses` and the lookups per `(dev, ino)` whenever a count is
off, and holds the counts exact in a run that lost nothing, repeating the run
(four attempts) until one does.

Then the loss was made on purpose. The session the test writes, run twelve
times beside 48 processes re-reading a cached file on 24 cpus:

| run | random lookups of 256 | sequential lookups of 1024 | `losses.dropped` |
|---|---|---|---|
| alone | 256 | 1024 | 0 |
| loaded, 11 of 12 | 241 to 253 | 990 to 1021 | 395 675 to 477 052 |
| loaded, 1 of 12 | 256 | 1007 | 416 875 |

Every short count came with a counted loss, and the reason is the kernel's
skipped runs (*"the kernel skipped a run of these programs because one was
already running on that cpu"*): a raw-tracepoint program runs with migration
disabled, so one preempted mid-run keeps its per-cpu recursion guard raised
and every later run on that cpu is skipped until it resumes. No run was short
without a loss, and the one loaded run that lost 416 875 and was exact says
where a skipped run lands: on whichever task's run it was, across the machine.
One loaded run read **0** sequential lookups against 1014 pages filled.

That is also why the loss is no slack. It counts every task's skipped runs,
not the workload's -- 395 000 to 477 000 against a shortfall of 3 to 15 of
256 -- so `lookups + lost >= asked` holds in any lossy run, and a silent loss
in `io_pagecache.rs`'s drain would pass beside it. The first version of this
item carried it as slack, and a review said so; the test now:

- in a run that lost nothing, holds the random reader's lookups to exactly
  256, both files' fills to exactly what `mincore` saw become resident, the
  sequential reader's lookups to at least half of 1024 (a read's first batch
  may go unseen, never counted twice), and the separation between the two
  readers;
- in a lossy run, holds only what a loss cannot excuse -- nothing over,
  `ahead` the fill minus the lookup, every row inside its phase -- prints the
  counts and the loss, and runs again;
- after four lossy attempts, says it did not measure the exact counts or the
  separation, with each attempt's loss, and stops: a skip, loudly, not a pass.

Alone on this machine, the first attempt is clean (3.2 s). A drain that lost
rows silently fails the first clean attempt.

What the kernel's own structure can still hide -- a read served by
`filemap_create_folio` after readahead's `__GFP_NORETRY` allocation failed,
which fires `mm_filemap_add_to_page_cache` but returns before
`mm_filemap_get_pages` -- was not seen here; the slack does not cover it,
because nothing reports it.

## 3. What the runner showed

Each checked against the CI logs first, and each now a skip with the
machine's reason where the machine is the cause:

- **`proc::status`'s first reading**: the runner read `collect::proc::` -- the
  forking test thread's name -- for `/bin/sleep`. An exec is not one instant:
  `/proc/<pid>/exe` names the new image before the kernel renames the task and
  closes its close-on-exec descriptors. `lifecycle::before_exec` now also
  treats a child whose name is still one of this process's thread names as
  inside its exec (bounded by the same 100 ms deadline, which a program named
  like one of `omn`'s threads pays). The test prints the collector's own
  account when it fails.
- **`scenario_proc`'s descriptor counts** (five tests, 22 against 20 and 30005
  against 30003): the five `proc-*.yml` workloads count what they inherit in
  their own `/proc/self/fd` *before* opening anything and add what they
  opened, and the tests hold the reported count to `>= 20` (`>= 30003`). Not
  counted after opening: a `listdir` of `/proc/self/fd` holds a descriptor
  while a 1 ms poller reads it, and read 30004.
- **The scenarios that assumed paranoid -1**: `scenario_sched`'s two switch
  tests skip on *"has no viable backend on this machine"* (paranoid 4) and on
  the new `pmu:sw` refusal (paranoid 2, below);
  `a_declared_mode_the_build_refuses_says_what_is_missing` no longer exists
  (the thirteenth wave made `count` served). `scenario_mem`'s counter sweep
  leaves out, and prints, the combinations the resolver itself withheld --
  31 on the runners, every cache source (no hardware PMU) and most of
  `mem.faults` -- because D5 is about a refusal raised after a backend was
  chosen; the two cache scenarios skip on the resolver's word.
- **A context switch is kernel mode, found on the way.** At paranoid 2,
  `sched.switches -m count` read **0** for a program whose threads counted 406
  switches: the counter layer reopened the counter user-mode only and noted
  it, and a switch never happens in user mode. The sampled mode was refused at
  start with `EACCES`. The `pmu:sw` factory now asks first
  (`kernel_mode_counted`) and refuses, before anything is launched, with the
  paranoia and the two ways out (paranoid 1, or `CAP_PERFMON`). The
  probe is P's (filed). The first version named a third -- *"CAP_BPF and
  CAP_PERFMON for the `bpf:*` backends ahead of this one"* -- which a review
  found wrong twice over: the `bpf:*` backends are *behind* `pmu:sw` in the
  chain since section 5, and they need `CAP_PERFMON` too, which is exactly
  what makes `pmu:sw` count kernel mode itself. And the check ran in the
  factory, whose refusal is classified as the build's (exit 3, *"the request
  itself is valid ... on a build that has it"*); it now runs at the attach
  (`KernelModeCounted`, wrapping either `pmu:sw` collector), where an
  unmarked refusal is the machine's (exit 1) and a launched program is still
  held at its gate. Not reproducible here (`perf_event_paranoid` is -1, and
  changing it would move five other streams' measurements); both modes of
  `pmu:sw` were run through the wrapper and count as before.
- **`scenario_io`'s eleven skips on `/dev/root`**: a mount source sysfs does
  not know is resolved through the mount table's own `major:minor` and
  `/sys/dev/block/<major>:<minor>`. And
  `substituted_counters_are_converted_exactly_and_their_gaps_are_named`
  named `zram0` as its appearing device, which the runner does not have: it
  is now a second real leaf device of the machine, and the test skips where
  there is only one.
- **`net.hwts` listed usable with no readable PTP clock**: `phc` and `ethtool`
  have probes now, answered by the collector's own start
  (`hwts::clock_support` and `Ethtool::open`), and `netlink:ethtool` requires
  `phc`. On the runner (`enP33055s1` keeps `/dev/ptp1`, root's alone) the
  source reads missing, with that sentence. The two sweep legs of
  `scenario_net` (`poll`, `snapshot`) first skipped whole on the resolver's
  word, which left every other source of the leg untested wherever
  `net.hwts` was missing -- a review's finding. `run_leg` now reads which
  source the resolver withheld (*"`<source>` has no viable backend on this
  machine"*), takes its requests out of the session, prints it, and runs the
  rest; `a_leg_without_a_withheld_source_keeps_every_other_request` checks
  the pruning here, where `net.hwts` is served: five requests of six and two
  of three, and each pruned session validates.
- **`family_net`'s two panics** without a packet socket or a user namespace
  are skips with that reason.
- **`writable:` for `mem.wss`**: `fs:proc:clear_refs` requires
  `writable:/proc/[pid]/clear_refs`, and the caveat that said it could not be
  a requirement says what it is.
- **`container: no` on `mem.cache_l1`, `_llc`, `_tlb`.** Their `poll` mode
  (the counters serve it since the thirteenth wave) is not taken: the entries
  did not ask for it and the sweep would grow by eighteen combinations for a
  mode no scenario measures.
- **`attach.rs`'s `online_cpus`** reads `capabilities::online_cpus`, and its
  own parser is gone.
- **Found in the CI's second run and not filed**: three `mem` unit tests
  (`swap`, `pressure`) waited thirty seconds for a child that never finished
  touching its memory, and the zero-page one read 23 MB of swap where 160 MB
  were due. A limited cgroup that cannot swap is killed or stalls. The tests
  now ask first (`Scratch::no_swap_room`: `/proc/swaps`' free room and every
  `memory.swap.max` up the tree, twice what the child must push out) and skip
  with the numbers; and a toucher that dies says how, with its cgroup's
  `memory.events`, instead of timing out.

## 4. The rest filed for S and N

- **`net.softirq`'s kernel count is a fact**: `the kernel's own /proc/softirqs
  counted ...` and `... handler runs were drained as N rows ...` come from
  `Collector::facts()` now and reach `notes`, not `degraded:`;
  `the_kernels_own_softirq_counter_bounds_what_this_run_bucketed` checks both
  sides.
- **`mem.numa` is keyed by node**: `KeyValue::node(n)` first on each event,
  `node` first in the entry's `keys`; `each_node_is_its_own_event` holds the
  key to the attribute and to `/sys/devices/system/node/online`.
- **`BPF_ENABLE_STATS`**: `crate::bpf::enable_run_time_stats()`, in
  `bpf/mod.rs` because `syscall.rs` is R's this wave; R's call can move onto it
  (filed).

## 5. `sched.switches` per cgroup: decided, and landed

**The `sched_switch` program exists, second in the chain, in `count` mode,
serving every target and reached first for a cgroup.** `src/bpf/switch_count.rs`
counts in the kernel per `(task, kind)`, the kind being `__schedule`'s own
rule -- voluntary when the switch was not a preemption and the state was not
`TASK_RUNNING` -- so the rows add up to `/proc`'s `nvcsw` and `nivcsw`;
`src/collect/sched/switch_count.rs` reports one event per task and kind over
the window. The catalog puts `pmu:sw` first for the targets it can follow and
`bpf:raw_tp` behind it with `modes: [count]` and a `cgroup` target, so:

- every existing `sched.switches` run costs what it cost: the resolver keeps
  `pmu:sw` wherever it serves;
- `--cgroup` is served, by the program, in `count` (278 rows for this
  session's own cgroup over 200 ms); `--cgroup -m trace` is refused with each
  backend's reason;
- the voluntary/involuntary split the caveat promised is reachable anywhere
  with `--backend force:bpf:raw_tp -m count`.

Why not first everywhere: on the question both backends answer -- how many
times a task left the cpu -- an inherited counter is exact and free, and the
program costs a run on every switch of the machine, narrowed after. Why not
`trace` too: the `woke` join from `sched.wakeup` needs a per-switch event,
and a counted row per drain is not one. And the reason `pmu:sw` was said to
be worth having -- no privileges -- is half true on a stock kernel: at
paranoid 2 it needs `CAP_PERFMON` like the program does (section 3).

Ground truth:

- `a_forced_switch_program_splits_the_switches_as_the_kernel_counts_them`:
  the switch fixture's four threads each read their own voluntary and
  involuntary counts; the program's are within the edges of each -- measured
  102/101, 104/102, 103/101, 102/101 voluntary and 0/0 involuntary.
- `a_cgroup_has_its_switches_counted_by_kind_and_none_of_the_one_beside_it`:
  the cgroup workload inside and its copy beside it; no row of the copy, the
  voluntary count between the sleeps that blocked inside the window (floor,
  less the counted loss) and the process's own lifetime `ru_nvcsw` (ceiling),
  the involuntary one under its lifetime `ru_nivcsw` -- measured 1673 in
  [1668, 1748] and 1778 under 1853, nothing lost.

## Needs from another stream

In `docs/open-findings.md`, "the fourteenth wave, stream K": the kernel-mode
counters' probe and refusal (P), one line out of `.github/expected-skips.txt`
(P), `pub mod net` and the statistics call (R), the preempted program's
skipped runs (the coordinator, `docs/bpf-layer.md`), `omn-mcp`'s three tests
that fail rather than skip without capabilities (J), and three for the flaky
tally (P's).

## The suite

One `make check` over the whole change stopped at
`family_sched::a_task_moved_between_two_cpus_is_counted_once_per_move`, #5 of
`docs/flaky.md`, which passed alone at once. The one `cargo test --workspace
--no-fail-fast` after it failed three tests, each with `No space left on
device` -- `/home` was full, from six streams' builds -- and each passed alone:
`scenario_sched::a_wakeup_from_an_interrupt_is_not_credited_to_the_task_it_landed_on`,
`triage::an_escalation_that_cannot_decide_anything_is_refused_before_the_launch`
and `triage::the_session_a_question_is_asked_through_is_an_ordinary_session`.
Everything else green, with `target/debug/omn` carrying `cap_bpf,cap_perfmon`
throughout; clippy (`--all-targets -D warnings`) and `cargo fmt --check` clean.

After the review's fixes (`a1bfd7c`): the first `make check` died linking,
`ld terminated with signal 7` with `/home` at 66 MB free; this worktree's
`target/debug/incremental` (1.8 GB, rebuilt on demand) was removed and the
check rerun with `CARGO_INCREMENTAL=0`. It went red in `scenario_net` at a
load of 12, four tests short of events the kernel's own counters had --
`every_datagram_sent_to_a_closed_port_is_a_drop_with_one_reason` (12 drops of
the datagrams sent; also seen once alone earlier, then green alone),
`the_ebpf_leg_of_the_sweep_answers_with_every_emit_it_promised`
(`connections` produced nothing), `a_queue_that_delays_every_packet_is_measured_as_that_delay`
(0 of 200) and `every_segment_the_kernel_resent_is_one_this_run_saw` (#flaky,
2 of 3). None is a test this change touched; `scenario_net` alone right after
was 39 of 39, and the one `cargo test --workspace --no-fail-fast` after it was
green throughout (46 test binaries), with clippy and `cargo fmt --check`
clean and `target/debug/omn` carrying `cap_bpf,cap_perfmon`. The first three
are for the flaky tally: an eBPF count short under load, the shape of the
preempted program's skipped runs (filed for the coordinator).

A warning for whoever runs a wave on this machine next: with `/home` full,
moving `target/debug` to `/tmp` looks like a way out and is not one. `/tmp`
is mounted `nosuid`, so file capabilities on a binary there are ignored, every
eBPF test skips as unprivileged, and nothing says why but the skips; and a
`target/debug` on disk with `deps` elsewhere makes cargo copy the binary
rather than hardlink it, which drops the capabilities on every `cargo test`.

## Index rows

No session file was added. Six were changed, and their rows stand:
`proc-sweep.yml`, `proc-exit.yml`, `proc-one-reading.yml`, `proc-handles.yml`
and `proc-loss.yml` (the workloads count what they inherit), and
`sched-forced-backends.yml` (it pins `sched.switches`' `bpf:tp` in `count`,
since `bpf:raw_tp` is written now).

## For `DECISIONS.md`

- **Closed: `sched.switches` gets a `sched_switch` program, second in the
  chain.** It serves every target in `count`, and is reached first for a
  cgroup, which `pmu:sw` cannot follow into; `pmu:sw` stays first wherever it
  serves, because on the question both answer an inherited counter is exact
  and free and the program costs a run on every switch of the machine. The
  program's rows carry the kernel's own voluntary/involuntary split, reachable
  anywhere with `--backend force:bpf:raw_tp -m count`. `trace` stays `pmu:sw`'s:
  the `woke` join needs a per-switch event. This is the first chain whose order
  is set by cost where two backends answer the same question and one answers
  more; "the backend that answers best first" holds for the question asked.
- **A counter of a kernel-mode event that the machine lets this process open
  for user mode only is refused, not noted.** A context switch never happens
  in user mode, and the count it would give is zero.
- **An `fentry:` requirement carries its prototypes** like a raw tracepoint's
  (`fentry:name(a, b)|(c, d)`); a bare name asks for existence only.
  (`CATALOG.md` §1's notation list.)
- **A requirement a collector's own start decides is probed by that start**:
  `phc` by `hwts::clock_support`, `ethtool` by resolving the family.
- **A loss counted over the whole machine is not slack for one workload's
  count.** An eBPF program's skipped runs (`fentry::skipped`) are every
  task's; a scenario whose count they would excuse holds the count exact in a
  run that lost nothing, repeats the run until one does, and says so when
  none did. The read-ahead lookup is the first.
- **A sweep leg leaves out the source the machine cannot serve, not itself.**
  Like `scenario_mem`'s withheld combinations: printed, and the rest asked.
- **A factory does not refuse for a machine's reason.** Its refusal is the
  build's by classification; a check that asks the machine runs at the attach.

## Filed for another stream

The items of `docs/open-findings.md`, "the fourteenth wave, stream K":
P (the kernel-mode counters' probe and refusal; a pair no backend declares worded as the request's; one line out of
`.github/expected-skips.txt`), R (`pub mod net`; `enable_run_time_stats`),
the coordinator (`docs/bpf-layer.md`: a preempted program skips every run
behind it), J (`omn-mcp`'s three tests that fail rather than skip without
capabilities), and three for the flaky tally (P's `cpu` tests).

## The matrix cells this should move

Not run (the coordinator's, at the end of the wave):

```console
scripts/matrix.sh here                                   # here-caps, here-nocaps
scripts/matrix.sh v5.15.221 v6.1.188 v6.6.157 --configs caps
```

- **Every cell with BTF: `net.tcp.connect`'s `fentry:tcp_v4_connect(...)`**
  stays *available* (5.15, 6.1, 6.6 and 6.17 declare `sockaddr *`, 7.2
  `sockaddr_unsized *`); a kernel with a third shape would move it from
  available to missing, by name.
- **`here-caps` and every `caps` cell on 6.1 and later: `sched.switches
  --cgroup -m count`** from refused (no backend declared the target) to served
  through `bpf:raw_tp`; `sched.switches --cgroup -m trace` stays refused, now
  with each backend's reason (worded by the resolver as the machine's, which
  it is not: filed for P). 5.15 is below the program's `kernel>=6.1`.
- **Any cell whose interfaces keep no readable PTP clock** (every guest, and
  the CI's runner): `net.hwts` from usable to missing, *"no interface on this
  machine keeps a readable PTP hardware clock"*; `here-*` unchanged on this
  machine, whose `/dev/ptp0` and `/dev/ptp1` are the `clock` group's.
- **The CI cells** (`ci-6.17.0-nocaps`, both configurations), read back by the
  coordinator: the fentry and `proc::status` unit tests green; `scenario_io`'s
  eleven `/dev/root` skips gone; `scenario_proc`'s five, `scenario_sched`'s
  three, `scenario_mem`'s three, `scenario_net`'s two and `family_net`'s two
  failures now passes or skips with the machine's reason; the `mem` swap and
  pressure tests skip on the runner's swap room, if that is what it was, or
  say how the child died. `scenario_net`'s two sweep legs run there without
  `net.hwts`, printed, and check the other five and two sources; at paranoid
  2, `sched.switches` through `pmu:sw` is refused with exit 1 (the machine's),
  not 3.
