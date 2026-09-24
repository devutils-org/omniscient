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
| `sched.mutex` | `preload` | implemented in the ninth wave: every pthread mutex acquisition, its wait and its hold, through the shim that interposes `malloc` |
| `sched.mutex` | `bpf:uprobe` | declared, not written |

**All ten of the family's sources collect** since the ninth wave. The last,
`sched.mutex`, went through the preload shim, and the section at the end of
this file is its record. `sched.futex` measures the contended half of the same
waits one layer down: every contended `pthread_mutex_lock` ends in a `futex`
syscall. What it cannot see is the uncontended majority, which never reaches
the kernel, or which *mutex object* was contended when a lock keeps its futex
word elsewhere; `sched.mutex` sees both, and cannot see a lock that is not a
pthread mutex at all.

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

# `sched.mutex` — the ninth wave, through the preload shim

The source the sixth wave left as "described, not built" is built, through the
mechanism the brief pointed at: the `LD_PRELOAD` shim that interposed `malloc`
now also interposes `pthread_mutex_lock`, `pthread_mutex_trylock`,
`pthread_mutex_unlock`, `pthread_cond_wait` and `pthread_cond_timedwait`. The
shim's half is `shim/src/mutex.rs`, the reader's `src/collect/sched/mutex.rs`,
and the pipe they share with the heap sources is
`src/collect/mem/heap/mod.rs`. The `bpf:uprobe` fallback is still vocabulary;
`examples/sched-not-implemented.yml` pins it and exits 3.

## What an event is

One per acquisition, stamped when the thread **asked** for the mutex, carrying:

| Attribute | Meaning |
|---|---|
| `wait_ns` | the value: from the call to `pthread_mutex_lock` until it returned with the mutex |
| `hold_ns` | from then until the release -- absent, not zero, when the hold was never timed |
| `addr` | the mutex, as an address in the program |
| `how` | `lock` or `trylock` (a successful trylock waited zero by construction) |
| `released_by` | `unlock`, `cond_wait` (the hold ended where a condition wait began), or `unseen` |
| `tid`, `pid`, `comm` | who |

The shim writes the record when the mutex is **released**, which is the first
moment both halves are known, and it writes it *after* the real unlock has
returned: a record written inside the critical section would lengthen every
hold by one `write(2)` and make every waiter pay for the measurement. What a
thread holds is a stack of eight in thread-local storage -- touched on every
lock and unlock, so it must not allocate -- and a thread nesting deeper has
its extra acquisitions reported at once with `released_by: unseen`.

## What each of the three lock sources sees

| | `sched.mutex` | `sched.futex` | `sched.lock` |
|---|---|---|---|
| where | in the call, in user space | the `futex` syscall | the kernel's own lock primitives |
| an uncontended pthread mutex | **yes**, at ~25 ns | no: it never reaches the kernel | no |
| a contended pthread mutex | yes, by mutex address | yes, by futex word | no |
| a condition variable, a semaphore | only the hold that led into the wait | yes | no |
| Rust `std::sync::Mutex`, Go, anything waiting on a futex directly | **no** | yes | no |
| glibc's internal locks (stdio, malloc) | no: they do not go through the exported symbol | yes when contended | no |
| a process already running | no: a preloaded library is given at exec | yes | yes |

The two measured facts that make the table more than a claim:
`examples/sched-mutex-uncontended.yml` has 1000 acquisitions and `sched.mutex`
reports all 1000, each waited for in under 100 ns; the same program under
`sched.futex` has no futex call to time. And a Rust program is invisible here
and says so -- the note the run prints names `sched.futex` as the source that
does see it.

## Ground truth

The fixture is a C program in `tests/scenario_sched.rs`, built by the test
into `target/omn-shim-fixture/mutex`. Its command line chooses the threads,
the acquisitions per thread and the hold; it holds by spinning rather than
sleeping, so the hold is the one it was given and not what the timer slack made
of it; and it times its own waits and holds with `CLOCK_MONOTONIC` and prints
the totals. Because the shim is inside the call the program timed, the tool's
interval is **nested** in the program's: its wait can only be shorter and its
hold only longer. That turns a second reference into two inequalities that hold
on any machine, loaded or not.

`examples/sched-mutex-known-hold.yml`, two threads × 500 acquisitions × 200 us,
on the development machine:

```
fixture: acquisitions=1000 wait_ns=101434977 hold_ns=200218553 min_hold_ns=200023
sched.mutex  1000 events over 202.72ms, no loss
  task:900540  n=500  min 20.00ns  mean 202.09us  max 101.03ms
  task:900541  n=500  min 20.00ns  mean 428.73ns  max 201.43us
```

What is asserted, and why each is safe on a shared machine:

- **exactly 1000 events, all on the fixture's one mutex** -- a count, not a
  rate, so load cannot move it;
- **every hold at least 200 us** -- the fixture spins until it is, and the
  tool's hold encloses the program's;
- **the tool's total wait is at most the program's, and its total hold at least
  the program's and within 10 % of it** -- the nesting, both ways.
- **not** a floor on the waits, and the reason is the most interesting
  number in the section. The argument for one looks sound: the threads start
  behind a barrier, so whichever finishes last was waiting through every hold
  of the other, 500 × 200 us = 100 ms at least. On an idle machine it held,
  101 ms. Inside the full suite the same run measured **54 ms, and the
  fixture's own clock said 55 ms**. A thread that is runnable with no cpu to
  run on -- before its first lock, or between an unlock and the next -- is
  neither waiting nor holding, and on a machine running two other streams'
  suites that is where the other 46 ms went. The tool and the program agreed;
  the expectation was about the scheduler, and the assertion came out.

**A glibc mutex is not fair**, which is the other reason an even split is not
asserted either. The thread that releases can take the mutex again before the
one it woke gets a cpu, so on the idle run one thread ran its 500 acquisitions
nearly back to back while the other waited once, for 101 ms, and then ran its
500 uncontended. A test asserting "each thread waits about half the time"
would be asserting a scheduling policy glibc does not have.

`examples/sched-mutex-uncontended.yml` is the same program with one thread and
the same thousand 200 us holds. The holds are the same; the waits vanish (26 ns
on average), and the scenario asserts the contended run waited at least a
hundred times longer. That is the separation `sched.futex` cannot make.

## The loss path, exactly

`examples/sched-mutex-loss.yml`: eight threads, each with a mutex of its own,
200 000 acquisitions each with no hold, so nothing slows the program but the
program. The pipe holds a megabyte, about 26 000 records of 40 bytes, and is
drained every 10 ms, so most records cannot be kept:

```
fixture: acquisitions=1600000
sched.mutex  189141 events, LOSS: 1410859 dropped
```

189 141 + 1 410 859 = 1 600 000, exactly. The shim drops rather than blocking
the program, counts what it drops **per class** -- a lost mutex record is not a
lost allocation, and the heap sources reading the same pipe are not charged for
it -- and flushes the count at exit. So the property is an equality whatever
the machine's speed, and the scenario asserts it; on a machine fast enough to
drain everything it still holds, and the test says the path was not exercised.

## One pipe, two classes

`examples/sched-mutex-with-heap.yml` asks for `sched.mutex` and `mem.heap.live`
on one launch. Since the eighth wave a launch has one shim stream; since this
one the stream carries classes. Each preload collector subscribes to a class,
the program is told the union in `OMN_SHIM_REPORT` (`3` here: heap and mutex),
and a hook whose class was not asked for calls straight through and writes
nothing -- a heap-only run does not pay for lock records, and a mutex-only run
does not fill the pipe with allocations. Each reader skips the others' records
without counting them as undecoded, and the scenario asserts both readers
finish with zero undecoded and zero dropped.

The descriptor variable is `OMN_SHIM_FD` now. It was `OMN_HEAP_FD`, which
stopped being true the moment the pipe carried a lock.

## What the shim costs here, measured

Two million uncontended acquisitions of a private mutex by one thread, wall
clock, three runs each, release build of the shim:

| | ms | per acquisition |
|---|---|---|
| bare | 179–184 | -- |
| preloaded, mutex class not asked for | 186–192 | +3.5 ns |
| preloaded and reporting, to `/dev/null` | 1007–1033 | **+415 ns** |

The 132 ns the brief quotes is the heap's figure from wave two, and it is not
this hook's. Almost all of the 415 ns is the one `write(2)` per record: the
hooks themselves are two clock reads and a thread-local stack. That is still
under the 740 ns a uprobe costs, but not by the margin the brief's reasoning
assumed, and on a program taking a lock per request it is not free. What
would change it is batching records in a per-thread buffer and writing them in
chunks, which trades a write per record for records that can be lost at thread
exit and arrive out of order -- a decision rather than a fix, left open.

`make` builds the shim in debug, where the same row is +760 ns: a measurement of
lock overhead should use `make release`.

## What is not measured, and why

- **Uncontended acquisitions are reported at the same price as contended
  ones.** A filter `wait_ns > 1000` runs after reading, not in the shim; the
  shim cannot know at unlock time what the reader will keep, and a threshold
  pushed into it would need a variable the request model has no way to send.
- **The reacquisition inside `pthread_cond_wait` is not an event.** Its wait
  for the mutex and its wait for the condition happen inside one call and
  cannot be separated from outside it. The hold that led into the wait is
  reported, ending there (`released_by: cond_wait`).
- **`pthread_mutex_timedlock` and `pthread_cond_clockwait` are not
  interposed.** A program using them has those acquisitions missing, and their
  unlock finds nothing on the stack and reports nothing, which is a count
  short rather than a wrong number.
- **A failed `pthread_mutex_trylock`** is a sign of contention and not a wait,
  and is not reported.
- **No stacks.** The catalog declares `stack` for this source because the
  uprobe backend could take one; the shim refuses it by name, exit 3, pointing
  at `sched.futex`, whose stacks show where a contended mutex blocked.
- **No process already running.** Refused, exit 3, with the reason. The route
  to one is in `docs/open-findings.md` under stream A's USDT requests: glibc on
  this machine publishes `libc:mutex_entry`, `mutex_acquired` and
  `mutex_release`, and a `bpf:usdt` backend on them would reach a running
  process with no preload. That is the backend worth adding before the uprobe.

## Found here, belongs elsewhere

Filed under "Needs from another stream — the ninth wave, stream S" in
`docs/open-findings.md`:

- **A loss reason beside clean counters is never rendered.** The heap stream's
  "no allocation was reported for pid ...: statically linked, own allocator"
  sentence was recorded as a loss reason from the start and never once
  printed, because `encode.rs` prints reasons only when a counter is non-zero.
  Found by running `gpu.api` on a program the shim cannot see and getting a
  silent zero. Fixed on this side by moving the sentence to
  `Collector::notes()`, which every preload collector now implements.
- **`dynamic-linking` has no probe**, so every preload source reports its one
  requirement "unverified" on every run.
- **The `hist` warning names a kernel the backend does not have.** "`trace`
  with `hist` aggregates in userspace unless a budget lets the resolver move
  the histogram into the kernel" is printed for a preload backend, where there
  is no kernel side to move anything to.

## A running process, through glibc's probes (tenth wave)

The shim reaches a program only at exec, so until the tenth wave `sched.mutex
--pid` was refused. Glibc publishes `libc:mutex_entry`, `mutex_acquired`,
`mutex_release` and `cond_wait` as USDT probes, and `bpf:usdt` is now the
backend for a `pid` or `tid` target: `src/collect/sched/mutex_usdt.rs` loads
`src/bpf/usdt.rs`'s stream program on the thirteen sites this machine's
`/usr/lib/libc.so.6` carries and pairs the hits per thread and per mutex in
userspace. It writes the event the shim writes -- stamped at the ask, `wait_ns`
the value, `hold_ns` beside it -- so a session reads the same either way.

A launch stays on the shim: it is the same measurement for 132 ns instead of
three uprobes, and the backend declares `launch: "no"` so the resolver cannot
choose it there. (It would otherwise, today: the shim's `dynamic-linking`
requirement has no probe yet, and an unverified requirement never counts as
available.)

**Ground truth**, `a_running_process_is_measured_through_glibcs_probes`: the
mutex fixture started by the test, two threads sharing a 2 ms hold, measured
by a run that attaches 200 ms later. Every hold that ended in an unlock is at
least the 2 ms the fixture slept inside it, and one of the threads waits at
least half a hold. Measured by hand the same way: holds of 20.06 ms against a
20 ms sleep, waits of 1 to 3 us uncontended and 19 ms behind the holder.

**What it cannot see**, each counted in the notes: a successful
`pthread_mutex_trylock`, which has no probe, arrives as a release of a mutex
never seen taken; the reacquisition inside `pthread_cond_wait` is an
acquisition with no entry and is not an event, as with the shim; a mutex taken
before the probes went in is released unseen. A release that follows a
`cond_wait` hit for the same mutex is `released_by: cond_wait`. A mutex still
held when the run ends is an event with no `hold_ns`, `released_by:
still_held`. Unlike the shim, it also sees glibc's own callers of
`pthread_mutex_lock`: the fixture's 1000 acquisitions read as 1003.

## Index rows for the coordinator

For `examples/README.md` and `GUIDE.md`, `sched` table:

```
| `sched-mutex-known-hold.yml` | Two threads, one pthread mutex, a known number of acquisitions held for a known time. |
| `sched-mutex-uncontended.yml` | The same program with one thread: the same holds, and waits of nothing. |
| `sched-mutex-with-heap.yml` | Lock waits and allocations of one program, through one shim. |
| `sched-mutex-loss.yml` | Mutex records produced faster than the pipe drains, and every one accounted for. |
```

`examples/sched-not-implemented.yml` is rewritten, not added: it now pins
`sched.mutex`'s `bpf:uprobe`, the one `sched` backend left without a
collector. Its row may want the same words.
