# `sched`

The family that measures the time a program spends **not** running. A thread
blocked on a lock, on a disk, or on another thread costs nothing in a cpu
profile and everything in latency, which is why this is the family most often
missing from a performance investigation that went nowhere.

The family that also owns the eBPF layer: see `docs/bpf-layer.md`.

## State

| Source | Backend | State |
|---|---|---|
| `sched.wakeup` | `bpf:raw_tp` | implemented: one record per wakeup, both ends |
| `sched.offcpu` | `bpf:raw_tp` | implemented: blocked nanoseconds per task, counted in the kernel |
| `sched.offcpu` | `bpf:tp:stacks` | declared, not written: `bpf:raw_tp` carries the stacks instead |
| `sched.runqlat` | `bpf:raw_tp` | implemented: a log2 histogram built in the kernel |
| `sched.migrations` | `bpf:raw_tp` | implemented: counted per `(task, destination cpu)` |
| `sched.switches` | `pmu:sw` | implemented: one sample per switch, unprivileged |
| `sched.switches` | `bpf:tp` | declared, not written |
| `sched.lock` | `bpf:raw_tp` | implemented in the sixth wave: kernel lock waits, bucketed in the kernel, with user stacks |
| `sched.futex` | `bpf:raw_tp` | implemented in the sixth wave: userspace lock waits, filtered to the waiting operations in the program |
| `sched.irq` | `bpf:raw_tp` | implemented in the sixth wave: hard interrupt handlers, paired per cpu. **The hard half only** |
| `sched.numa_balance` | `bpf:raw_tp` | implemented in the sixth wave; cannot fire on a machine with one NUMA node, which this one is |
| `sched.mutex` | `preload`, `bpf:uprobe` | declared, not written: the only `sched` source left without a collector |

**Nine of the family's ten sources collect.** What is left is `sched.mutex`,
whose two backends are the two mechanisms this layer still does not have — a
shim that interposes `pthread_mutex_lock`, and a uprobe on a symbol resolved
inside the target's own libc. `sched.futex` measures the same waits one layer
down and does collect: every contended `pthread_mutex_lock` ends in a `futex`
syscall. What it cannot say is which *mutex object* in the program was
contended, only which futex word — the same address for an ordinary glibc
mutex, and not for a lock implementation that keeps its word elsewhere.

## Why `sched_waking` and not `sched_wakeup`

Both fire for the same wakeup. Only the first one runs in the waker's context.

`sched_waking` is raised by `try_to_wake_up` on the cpu of the task doing the
waking, so `bpf_get_current_pid_tgid()` inside it is the waker.
`sched_wakeup` can be raised on the woken task's cpu after an inter-processor
interrupt, where the current task is whatever happened to be running there. A
waker read from `sched_wakeup` is therefore wrong precisely on cross-cpu
wakeups — which are the interesting ones, because a cross-cpu wakeup is what a
thread pool, a lock handoff and an I/O completion all look like.

This is recorded because the catalog originally required both tracepoints, and
the choice between them looks arbitrary until it is written down.

## What a wakeup is not

`sched.wakeup` reports one event per wakeup and its unit is `count`, not `ns`.
The catalog said `ns`, and that was wrong in a way worth naming: a wakeup has
no duration. The nanoseconds this family is about appear only when a wakeup is
paired with what the woken task did next —

- wakeup → on cpu is the run queue wait, which is `sched.runqlat`;
- blocked → wakeup is the blocked time, which is `sched.offcpu`.

Reporting a wakeup stream in nanoseconds would have invited exactly the
reading the tool exists to prevent: a number that looks like a latency and is a
timestamp.

## Ground truth

- **`sched.switches`** is checked against `/proc/thread-self/status`, which
  counts the same switches from the other side of the kernel. The first version
  of that test read `/proc/self/status` and compared against a constant zero:
  `self` is the thread group leader, which in a test binary is asleep while
  another thread does the work, and a perf event follows the task that opened
  it. A ground-truth test that reads the wrong task is worse than none.
- **`sched.wakeup`** is checked against an injected, known count: a program that
  blocks *n* times is woken at least *n* times, because a timer expiring is a
  wakeup and there is no other way back onto the cpu.

Both floors, never equalities. The kernel is free to add wakeups — a signal, a
page fault on a cold binary, a preemption — and a test that demanded an exact
count would be asserting that nothing else happens on the machine.

## What the first privileged run measured

The program loads, the verifier accepts it, it attaches to `sched_waking`, and
the events are the causal chain they claim to be. Running
`omn sched.wakeup --emit raw -- /bin/sh -c 'for i in 1 2 3 4 5; do sleep 0.02; done'`
produced, in order: one wakeup of the launched process by `omn` itself -- the
launch gate being released, before the `exec`, so the woken task is still named
`omn` -- then five wakeups of the shell, each by a different `sleep` child
exiting. Five blocks, five wakers, named.

And one thing worth knowing before trusting a waker.

**A waker is the task that was running when the wakeup was issued, which is not
always the task that caused it.** A wakeup issued from interrupt or softirq
context is credited to whatever was interrupted, because that is what `current`
is there. Measured, with a process that does nothing at all:

```
$ omn sched.wakeup --emit raw -- /bin/sleep 0.3
  wakee kworker/1:2   waker 772820   (the sleeping process)
  wakee rcu_preempt   waker 772820
  wakee tailscaled    waker 772820
  wakee sleep         waker 0        (the idle task: a timer expiry)
```

A process asleep for 300 ms wakes nobody. Those three are the timer and network
softirqs running on its cpu, in its context. A waker of pid 0 is the idle task,
which is what a timer expiry looks like from here.

This is not a bug in the collector and not something `sched_wakeup` would fix --
it is what the kernel knows at that point. It is stated as a caveat on the
source, so it reaches the user beside their own numbers rather than living here.
Telling the two apart needs the interrupt state at the time of the wakeup, which
on x86 is a per-cpu `preempt_count` rather than a field of `task_struct`, so it
needs `bpf_this_cpu_ptr` and a ksym lookup the layer does not have yet. Until
then the waker is evidence, not proof.

## Off cpu is blocked, not preempted

`sched_switch` fires whenever a task leaves the cpu, and there are two reasons
it can: it was waiting for something, or the scheduler took the cpu away. Only
the first is blocked time. The second is run queue wait and belongs to
`sched.runqlat`, whose whole subject it is.

They are told apart by `prev_state`, the fourth argument of the tracepoint,
which is zero exactly when the task was still runnable. The program drops that
case in the kernel rather than carrying it to userspace to be filtered there,
because on a busy machine preemptions outnumber blocks and carrying them is
most of the cost.

The state is then kept rather than reduced to a boolean, and it reaches the
user as a word:

| `state` | what it was | why it is worth separating |
|---|---|---|
| `sleeping` | `TASK_INTERRUPTIBLE` | waiting for something to happen: a timer, a pipe, a socket |
| `uninterruptible` | `TASK_UNINTERRUPTIBLE` | usually waiting on a device, and usually the answer |
| `idle` | `TASK_UNINTERRUPTIBLE \| TASK_NOLOAD` | an idle kernel worker, which the load average deliberately ignores |
| `stopped`, `traced` | `__TASK_STOPPED`, `__TASK_TRACED` | stopped by a signal or by a debugger, which is not the program's own waiting |

Lumping the third into the second is how a machine doing nothing comes to look
like a machine in trouble, so `state_bits` carries the kernel's own value
beside the name and a state this build does not recognise reads as `other`
rather than as one it does.

Two edges the numbers cannot show on their own, both stated as caveats on the
source so they travel with the measurement:

- **A task still blocked when the run ends is in no total**, because its block
  has no end yet. A program that spends its life waiting would otherwise report
  almost nothing, so the count of them is reported with the run.
- **A task that leaves the cpu to die is dropped in the kernel.** `TASK_DEAD`
  never comes back, and without that filter every short-lived child of an
  observed program would sit in the in-flight map until the run ended.

## What an aggregating collector changes

`sched.offcpu` is the first source whose events are not one per thing that
happened. The kernel counts, userspace drains, and each event is a total over
the interval between two drains rather than one block.

That has two consequences worth stating rather than discovering:

- **The window is on the event.** `window_ns` says which interval the total
  covers. A number aggregated over an interval that does not say which interval
  cannot be compared with another.
- **A distribution does not survive it.** `hist` is declared on this source and
  a map holding a total and a count cannot produce one. The fix is in the key,
  not in the emit: add a log2 bucket to it and the histogram falls out of the
  map. That is written up under *Needs from the shared layer* below, because
  the bucket layout has to be the one the emit expects.

## What the first privileged off-cpu run measured

Every number below is off this machine, with `CAP_BPF` and `CAP_PERFMON` on the
binary.

**A known delay comes back to five decimal places.** `omn sched.offcpu --emit
raw -- /usr/bin/sleep 0.4` reports `total_ns=400053520` in one block: 400.05 ms
against 400 ms asked for, which is the scheduler's own timer slack and not
error in the measurement.

**A known number of device waits comes back to within one.** 128 MiB read with
`O_DIRECT` in 64 KiB blocks is 2048 reads that cannot be served from the page
cache, and the run reports `blocks=2047`, `state=uninterruptible`,
`state_bits=2` -- `TASK_UNINTERRUPTIBLE` exactly, with no other bit set. One
read did not have to wait.

**A program that only spins reports nothing.** Which is the assertion that
preemption is not being counted, and the one that would fail loudly if the
`prev_state` test were dropped.

**Machine-wide, 400 ms produced 177 entries** across every task on the machine
that blocked, including `omn`'s own tick sleep, and the thread-level split is
real: `tailscaled` appears as tid 1051 under pid 965, an `io-reader` thread as
tid 87458 under pid 87420.

Two things the states showed that a table of constants would not have:

- **`TASK_FREEZABLE` is set more often than not.** An ordinary `nanosleep`
  reports `state_bits=8193`, which is `TASK_INTERRUPTIBLE | TASK_FREEZABLE`,
  and a killable device wait reports `258`, which is `TASK_UNINTERRUPTIBLE |
  TASK_WAKEKILL`. Any reading of the state has to be a mask test; comparing it
  to `1` or `2` would classify almost nothing.
- **`idle` is common and would have been alarming.** Kernel workers sit in
  `TASK_UNINTERRUPTIBLE | TASK_NOLOAD` (`1026`), which the load average
  deliberately ignores. Reported as `uninterruptible` it would make an idle
  machine look like one stuck on a disk.

And one trap found while writing the test for the second measurement, which is
this project's recurring lesson in a new costume: **a file of zeros is not a
file to read.** The first version wrote 32 MiB of zeros and expected 512 device
reads; btrfs compressed them away and the run reported 133 waits. The collector
was right and the premise was wrong. The test now writes bytes from an
xorshift, and syncs them, so there is something on the device to wait for.

## Traps

- A switch count is a symptom. A *voluntary* switch is a task blocking on
  something; an *involuntary* one is the scheduler taking the cpu away. They
  call for opposite responses, and `pmu:sw` counts both as one. Only the eBPF
  backend, reading `prev_state` at `sched_switch`, can tell them apart — which
  is why `bpf:tp` is ahead of `pmu:sw` in the chain even though `pmu:sw` is free
  and always available.
- `PERF_COUNT_SW_CONTEXT_SWITCHES` counts switches *out*. Ten blocking waits are
  ten events, not twenty.
- The perf event does not follow children: `inherit` cannot be combined with a
  per-task ring buffer (`src/perf.rs` states this). Observing a shell that
  forks measures the shell.
- `sched.offcpu` is expensive on switch-heavy workloads unless it aggregates in
  the kernel, which is why it does: the program counts nanoseconds per
  `(task, state)` in a hash map and userspace drains a few entries per tick
  instead of reading a few million events. With stacks it will count per
  `(task, state, stack)` instead, and the stacks are what is still missing.

## What this family carries, and what it does not

`sched.wakeup` declares the `task` and `cgroup` keys. The `bpf:raw_tp` backend
carries `task` only. The cgroup of a *woken* task is not the cgroup of the
waker, so `bpf_get_current_cgroup_id()` would answer a different question than
the one the key names; it will be read from the woken `task_struct` when a
correlation actually needs it, and until then the key is absent rather than
approximated.

## The four the sixth wave landed

All four are the same shape seen four times: a pair of tracepoints that open
and close an interval, a log2 histogram built in the kernel, and a per-cpu
counter of what could not be recorded. What they share is in
`src/bpf/sched_common.rs`; what differs is the key, the filter and the pairing,
and each of those differs for a reason its own file gives.

### `sched.lock` — the locks the kernel takes

`lock:contention_begin` and `lock:contention_end`, which is why the floor is
6.1: `DECISIONS.md` chose that floor so this source would not need a kprobe on
every lock primitive, covering a different set on every kernel.

The pairing is by thread id and nothing else, because a thread waits for one
lock at a time — a thread blocked on a lock is not running, so it cannot begin
a second wait before finishing the first. What that made possible is a map with
a four-byte key; what it made *im*possible is the idle task, and that is the
first thing this collector got wrong. See "What is not measured" below.

The lock's `LCB_F_*` flags are part of the row key rather than a field carrying
their last value, and that turns out to be the whole measurement rather than a
refinement:

```
twelve writers appending to one file, two seconds

                       write (rwsem)              spin
  one shared file    2233 waits, 5.82 s     1602 waits, 16.3 ms
  twelve own files      0 waits                838505 waits, 18.4 s
```

The contended run has two hundred times *fewer* lock waits than the
uncontended one. Both numbers are right: serialising twelve writers on one
inode makes them slow, slow processes make fewer syscalls, and it is the
syscalls that reach the filesystem's own spinlocks. A count of lock waits is a
throughput measurement wearing a contention measurement's name, and the two go
opposite ways. What separates the two runs is which lock and for how long.

### `sched.futex` — the locks the program takes

`sys_enter` and `sys_exit`, narrowed in the program to the four operations that
make a thread wait. The narrowing matters twice over: `futex()` is one syscall
with a dozen operations behind an argument, and `FUTEX_WAKE` — the other half
of every wait — returns in a microsecond, so timing all of them would put the
wake path in the same distribution as the waits.

Two details are worth repeating because getting either wrong produces a
plausible number rather than an error. The `op` is read as thirty-two bits,
because the kernel takes `int op` out of a sixty-four bit register and whatever
userspace left above it is not part of the command. And the two flag bits,
`FUTEX_PRIVATE_FLAG` and `FUTEX_CLOCK_REALTIME`, are stripped by conditional
subtraction the way the kernel strips them, rather than by enumerating the
sixteen combinations — glibc sets the first on every private lock, so a program
matching the raw `op` would match no wait any ordinary program makes.

The ground truth is an injected delay: a lock held 250 ms, asked for 50 ms
late, three times. The run reports three waits of 199.87 ms, 250.04 ms and
250.04 ms, all three inside the `[134.2 ms, 268.4 ms)` bucket.

### `sched.irq` — the time nobody is charged for

`irq_handler_entry` and `irq_handler_exit`, paired **per cpu** rather than per
task. That is not an optimisation: an interrupt handler is not running on
behalf of the thread it interrupted, so keying it by the current task would
charge a disk's interrupt to whatever happened to be on the cpu. A hardware
interrupt handler runs with its line masked and does not nest, so the in-flight
timestamp is one slot of a per-cpu array — no hash, no key, no atomic on the
hottest path in this build.

The ground truth is `/proc/interrupts`, which the kernel maintains itself, in a
different place and for a different reason. Over three seconds:

```
 irq  tool  procfs  name
  70   321     321  nvme0q2
  69   154     154  nvme0q1
  71     5       5  nvme0q3
  84   387     439  iwlwifi:default_queue
```

The exact lines are the ones that were quiet at the edges of the window. The
check in `tests/scenario_sched.rs` is the inequality rather than the equality,
because the tool attaches after the file is read and detaches before it is read
again: what it counted is inside what the file counted, on any machine and
whatever it was doing.

### `sched.numa_balance` — the decision not to move

`sched_stick_numa`, counted per `(task, source cpu, destination cpu)`. The node
is added on the way out rather than in the kernel: the event carries cpus, and
the cpu-to-node map is in `/sys/devices/system/node`, which a program in the
kernel cannot read.

**This source cannot fire on this machine**, which has one NUMA node. There is
no balancing to do when there is nowhere else to be. An empty series from a
machine that cannot produce one and an empty series from a balancer with
nothing to do look identical, so every run on a single-node machine says which
it is, with the node count in it.

## What is not measured, and why

Each of these is a promise the catalog makes that this build does not keep in
full. They are here rather than only in the code because a limit a reader
discovers is a limit that has already cost them something.

**`sched.irq` is the hard half only.** The entry's summary says "hard and soft
interrupt handlers". Its `requires` name `irq_handler_entry` and
`irq_handler_exit`, which are the hard ones; softirq time — the network receive
path, block completions, the timer wheel — is `softirq_entry` and
`softirq_exit`. Attaching to a tracepoint the resolver never probed is how a
backend comes to be reported available and then fails at attach, which is
finding 15, so the gap is left open. Every run of this source says so in its
own output. What would close it is one line in `catalog/sched.yaml`, requested
under "Needs from another stream" in `docs/open-findings.md`.

**`sched.lock` does not measure the per-cpu idle task.** Every cpu's idle task
is numbered thread zero, so an in-flight map keyed by thread id puts
twenty-four of them in one slot, and a wait opened on one cpu is closed by
another cpu's end. Measured before the exclusion, that produced a wait of a
hundred and fifty million minutes, rendered in the histogram as calmly as any
other number. The idle task's contention is real and is in interrupt context;
what is not real is any duration this pairing could give for it. Each run
reports how many it set aside.

**`sched.lock` and `sched.futex` walk the user stack, never the kernel's.** The
kernel side of a lock wait is the lock primitive — the same few functions every
time — and the side that says something about the program is the call that took
the lock. It is also the only side this build can name: kernel frames need
`/proc/kallsyms`, which a normal machine restricts to root, and
`CAP_SYS_ADMIN` is more than the two capabilities this project settled on.

**No `sched` collector serves a `cgroup` target.** The catalog declares
`cgroup: bpf_only` for `sched.wakeup`, `.runqlat`, `.offcpu`, `.migrations`,
`.lock` and `.futex`, and every one of them refuses a cgroup target by name.
This predates the sixth wave and the new collectors inherited it rather than
each inventing a different answer. What it needs is
`bpf_get_current_cgroup_id()` and a cgroup id to compare against, which is a
shared-layer job.

**`sched.futex` does not see `futex_waitv`.** A program using the vectored wait
syscall — a Go runtime on a recent kernel, for instance — waits on several
futexes at once through a different syscall with a different argument shape.
This build filters on `futex()` alone and says nothing about the other, which
means a program that uses it measures as a program that never waits.

**A run's filter is applied to the drained rows, not in the program.** The
resolver credits any backend whose id begins with `bpf` with pushing a
predicate into the kernel, and all four of these collectors make that true of
the *numbers* by applying it themselves. It is not true of the *cost*: the rows
the filter removes were still built, and each run says how many. This is the
same gap `sched.runqlat` already records.

## Needs from the shared layer

The first four are the sixth wave's, and each is written up with the signature
it wants under "Needs from another stream" in `docs/open-findings.md`.

- **`insn::helper` does not list `bpf_get_current_comm` or
  `bpf_probe_read_kernel_str`.** Both are needed by three of the four
  collectors this wave landed, and both are named locally in
  `src/bpf/sched_common.rs::extra_helper` rather than in `insn.rs`, which five
  streams share. They are numbers in the kernel's `bpf_func_id` enum and belong
  in the shared list.
- **There is no instruction for a bitwise `and`.** `insn.rs` has `rsh64_imm`
  and `jset_imm` and no `and64_imm`, so `sched.futex` strips the two futex flag
  bits by conditional subtraction — correct, and four instructions where one
  would do. Anything masking a field will want the same.
- **There is no variable-offset read of a tracepoint record.** A classic
  tracepoint's `__data_loc` fields hold a packed offset and length, and reading
  the string they point at means a bounded read at an offset the verifier has
  not seen before. `gpu.job` wants one for the DRM ring's name and does without
  it; nothing else in this build has met one yet.
- **`attach.rs` finds the tracefs root and does not publish it, and nothing
  reads a `format` file.** `src/bpf/sched_drm.rs` has both, written locally
  because `attach.rs` belongs to nobody. Whoever else takes the classic
  tracepoint path will write them a third time.
- **G5, `hist` and `topk`.** `sched.runqlat` and `sched.offcpu` are histogram
  sources; both are declared with `emits: [hist, ...]`. `sched.offcpu` collects
  without it -- a total and a count per task are useful on their own -- but the
  distribution needs the bucket in the aggregation key, so the layout `hist`
  settles on is a decision this family is waiting for rather than a rendering
  choice.
- ~~**G6, stack capture.**~~ Landed: `bpf_get_stackid`, a stack map and the
  symbolizer are in the layer, and `sched.offcpu`, `sched.lock` and
  `sched.futex` all put the stack id in the key of the map their programs
  keep.
- **G8, perf counting mode.** `sched.switches --mode count` is refused today
  with that reason: a counting request should open no ring and read the counter
  at the end, and sampling every switch to add the samples up would be a
  different, more expensive measurement wearing the cheap one's name.
- **`join`: the `wakeup` match kind.** The join engine implements interval
  matching only, and refuses `wakeup` by name. `sched.wakeup` exists to feed
  it; until the match kind lands, the events are correct and uncorrelated.

## Found here, belongs elsewhere

Two findings from this thread's runs that are not this family's to fix.

**`perf::tests::a_page_fault_sample_carries_the_address_that_faulted` is flaky,
and the collector is not at fault.** It touches 64 pages of a fresh `Vec` and
demands 64 faults. Run alone it gets them; run inside the full suite it can get
two, and reports no loss, which reads as a silent shortfall. Measured here:

```
first allocation:  64 faults
second allocation: 64 faults
third allocation:   0 faults
```

The allocator hands back pages that are already resident, so touching them
faults nothing. The test's premise -- that touching a page faults -- holds for
fresh memory only, and a suite that has already allocated and freed leaves the
arena warm. A fix belongs with the `mem` thread or with `src/perf.rs`: map the
block with `mmap(MAP_ANONYMOUS)` so the pages are guaranteed fresh, or check
the sample count against the minor-fault delta in `/proc/thread-self/stat`
instead of against a constant. This is invariant 5 in miniature -- the
surprising number was right and the expectation was wrong.

**A throttled perf event loses samples silently.** `PERF_RECORD_THROTTLE` says
the kernel decided an event costs too much and sampled less of it, up to
`kernel.perf_event_max_sample_rate`. `SampledEvent` decodes the record and
`src/collect/mem/page_faults.rs` ignores it, so a throttled run looks complete.
`sched.switches` counts it into `Losses::dropped` with a reason naming the
sysctl; the shared layer would do better to expose `SampledEvent::throttled()`
so every perf-based collector gets it, and `Losses` to carry a `throttled`
count of its own, since the number of lost events in a throttled period is not
known.

## Shared files this thread had to touch

Listed so the coordinator can see them at a glance; both are one-liners that a
family collector cannot avoid.

- `src/lib.rs`: `pub mod bpf;`.
- `src/capabilities.rs`: the `bpf` and `raw_tracepoint:<name>` probe classes,
  which are G4 and belong to the layer this thread owns. Both delegate to
  `src/bpf/`; no probing logic was added to `capabilities.rs` itself.
