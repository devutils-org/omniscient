# `proc` — implementation notes

> Two waves, in order. Everything down to the rule below is the first wave's
> record, kept as it was written; **the four sources it calls blocked have since
> landed**, and the second half of this file starts at
> *"The four event sources, over eBPF"*. Read the status table there for what is
> true today.

## Status

| Source | Backend | Ground truth |
|---|---|---|
| `proc.fd` | `fs:proc` | a launched process reports its own descriptor count; the collector must match it exactly |
| `proc.threads` | `fs:proc` | same, for threads |

Blocked on the eBPF layer: `proc.syscall`, `proc.exec`, `proc.exit`,
`proc.signals`. *(No longer: all four landed in the sixth wave. Their row of the
status table is in the second half of this file.)*

## Findings

### These are the only counts in the catalog that can be checked exactly

Everything else in this tool is measured with skid, sampling, or a folio size in
the way. A process holding seventeen descriptors holds seventeen. So the tests
assert exact equality rather than a tolerance, and they compare two independent
readers of the same truth: the measured process counts its own descriptors and
threads and reports them, and the collector reads the same kernel directories
from outside.

### The exact test cannot live in the unit tests

`cargo test` runs tests in parallel threads of one process, so other tests open
files and start threads while a unit test tries to measure a delta. The first
attempt asserted "open seventeen files, the count rises by seventeen" against
this process and failed, correctly. The exact comparison moved to
`tests/family_proc.rs`, against a launched process that nothing else touches;
the unit tests keep the claims that survive a shared process.

### Reading your own descriptor table is not like reading someone else's

The handle opened to walk `/proc/<pid>/fd` appears in that listing when the pid
is your own, and does not when it is not. The first implementation subtracted it
unconditionally, which was right for the unit test measuring this process and
wrong by one for every real target. The exact ground truth against a child is
what caught it — a tolerance of plus or minus one would have hidden it forever.

### A count without its limit is not a measurement

900 open descriptors is comfortable against a limit of 524288 and an outage
against a limit of 1024. The soft limit and the headroom travel with the count,
and a test asserts all three are present.

## Needs from the shared layer

*(Both were answered by the sixth wave; what is still wanted is at the end of
this file.)*

- The eBPF layer, for the other four sources. `proc.syscall` in particular needs
  in-kernel aggregation before it is usable: `raw_syscalls` fires on everything,
  and a raw trace of it on a syscall-heavy workload is a load test rather than a
  measurement.
- `proc.exec` and `proc.exit` want the `proc:pidns+pid` key rather than
  `task:tid`, since a tid alone is ambiguous the moment containers exist. The
  key exists in the model and nothing produces it yet.

---

# The four event sources, over eBPF — the sixth wave

`proc.exec`, `proc.exit`, `proc.signals` and `proc.syscall` collect. All six
sources of the family now have a collector, and `omn list proc` says so:

```
SOURCE        MODES           COST   DET     HERE  COLLECTOR  SUMMARY
proc.syscall  trace, count    high   low     yes   1 of 2     Per-syscall latency and count
proc.exec     trace           free   medium  yes   1 of 2     Process execution events
proc.exit     trace           free   medium  yes   1 of 2     Process exit events with exit code
proc.fd       poll, snapshot  free   medium  yes   all        Open file descriptors against the limit
proc.threads  poll, snapshot  free   medium  yes   all        Thread count
proc.signals  trace, count    low    medium  yes   1 of 2     Signals delivered
```

`1 of 2` rather than `all` is honest and deliberate: each of the four declares a
`bpf:tp` fallback that nothing collects, and `examples/proc-not-built.yml` pins
it so that the family keeps a demonstration of exit 3. That exit is 3 where
tracefs is readable, and 1 where tracefs is root-only: `force:` skips the
resolver's choice, not the pinned backend's probes, and a missing requirement
is refused before a missing collector.

## Status

| Source | Backend | Ground truth |
|---|---|---|
| `proc.fd` | `fs:proc` | a launched process reports its own descriptor count; the collector must match it exactly |
| `proc.threads` | `fs:proc` | same, for threads |
| `proc.exec` | `bpf:raw_tp` on `sched_process_exec` | a copy of `/bin/sh` under a name the test invented, executed a chosen number of times; the count of events carrying that path must be exactly that |
| `proc.exit` | `bpf:raw_tp` on `sched_process_exit` | the same program told to `exit 7`, and another killed with `SIGKILL`; the status and the signal must come back as the two different things they are |
| `proc.signals` | `bpf:raw_tp` on `signal_generate` | a workload that sends itself a chosen number of real-time signals; counted plus dropped must close against that number |
| `proc.syscall` | `bpf:raw_tp` on `sys_enter` and `sys_exit` | a thousand `getppid` calls, counted exactly, and ten sleeps of fifty milliseconds whose mean must be the delay that was injected |

Where the code is: `src/bpf/proc_events.rs` holds the three streaming programs,
`src/bpf/proc_syscall.rs` the aggregating pair, `src/collect/proc/events.rs` and
`src/collect/proc/syscall.rs` the collectors, and
`src/collect/proc/syscall_names.rs` the syscall table.

## Findings

### An exec event is where the family's three oldest defects came from

Defects 1 and 2 of `docs/scenarios/proc.md` are both races created by inferring
a launched program's lifecycle from `/proc`: before the child reaches `execve`
its entry still describes `omn`'s own forked image, and after it exits the entry
describes a zombie that reads successfully and holds nothing.
`sched_process_exec` and `sched_process_exit` are those two instants published
by the kernel instead of guessed at from outside, and they cost nothing — the
catalog calls both `free` and a machine-wide run of the pair measured 2.2 ms of
cpu over 700 ms of wall time.

They do not *fix* those defects, and this stream did not pretend otherwise:
`proc.fd` is a different source with a different mechanism and its fix, in
`src/collect/proc/lifecycle.rs`, has landed already. What these two add is the
evidence a future fix could be built on, and a reader who wants to know when a
program really started can now ask.

### The kernel packs a status and a signal into one word, and reporting it raw is wrong

`task_struct.exit_code` holds `status << 8` for a task that returned and the
signal number in its low seven bits for one a signal killed. A build that
reported that word as the exit code would say a program killed by `SIGKILL`
exited 9, and a program that genuinely returned 9 carries 2304 in the same
field. So the event carries three things rather than one:

- `code`, the shell's convention — the status for a program that returned, and
  `128 + signal` for one a signal killed. It is a convention, it is named as
  one, and the catalog declares `code` as the filter field so something has to
  be under that name.
- `status` and `signal`, the two unambiguous halves. `status` is **absent** from
  the event for a signalled death rather than zero, because zero is a real exit
  status and a reader filtering `status == 0` must not catch a program that was
  killed.
- `exit_code`, the kernel's own word, undecoded, so the packing stays visible.

Measured: `sh -c 'exit 7'` gives `code=7 status=7 signal=0 exit_code=1792`, and
`sh -c 'kill -9 $$'` gives `code=137 signal=9 exit_code=9` and no `status`.

### `sched_process_exit` fires once per thread, not once per process

A count of exit events is a count of *threads* until it is filtered. The second
argument of the tracepoint is `group_dead`, which is set only on the last thread
of a process, and it is on every event for exactly that reason. The collector
says so in its own notes rather than leaving it to be discovered: a machine-wide
second measured 243 threads leaving and 185 processes.

### Five identical signals to one process generate five and deliver one

The clearest measured surprise of this stream, and it is the kernel being right.
`signal_generate` fires when a signal is *generated*; the tracepoint's fifth
argument says what became of it. Sending `SIGUSR1` five times to a process that
has not handled the first gives five events, of which one has `result=0` and
four have `result=1` — a non-real-time signal that is already pending is not
queued again.

```
sig=10 signame=SIGUSR1 ... result=0 delivered=1
sig=10 signame=SIGUSR1 ... result=1 delivered=0
sig=10 signame=SIGUSR1 ... result=1 delivered=0
sig=10 signame=SIGUSR1 ... result=1 delivered=0
sig=10 signame=SIGUSR1 ... result=1 delivered=0
```

So *"signals delivered"*, which is the catalog's summary of this source, is what
`delivered` counts and not what the event count counts. Both are on the event.
The one that would be wrong is reporting only the event count under that name.

Real-time signals do queue, which is what makes the loss scenario possible at
all: three hundred thousand `SIGRTMIN` are three hundred thousand records.

### The syscall histogram is built in the kernel, and there was no other option

`raw_syscalls:sys_enter` fires on every syscall of every process. Measured on an
ordinarily busy development machine: **31 451 calls in one second**, reported as
5 015 rows, for 26.9 ms of cpu — 1.81 % against the machine's own 1.49 s. The
streaming shape would have published 31 451 ring records for the same second and
the numbers would have started describing `omn`. `src/bpf/runqlat.rs` is the
worked example this follows, down to the unrolled six-step log2 and the
`emit::WEIGHT_ATTR` on each row.

Two things the kernel-side shape costs, both stated in the run's own output:

- **A bucket is a factor of two wide**, so every number derived from `lat_ns` is
  bucketed. `total_ns` is kept beside the count for exactly this reason: the
  mean of ten fifty-millisecond sleeps is 50.03 ms from `total_ns` and "between
  33.5 and 67 milliseconds" from the bucket.
- **A thread inside a syscall when the run ends is in no bucket**, because its
  call has no end yet. A machine with threads parked in `epoll_wait` always has
  some — 193 of them in the run above — and that is a fact about the window
  rather than a loss, so it is a note and not a loss counter.

### The filter had to run in the kernel, and the claim had to be checkable

The resolver credits any backend whose id begins with `bpf` with pushing
predicates down, and writes `"placement": "kernel"` into the resolved file kept
beside the numbers. For every other source in this build that credit is about
the numbers; here it is about the cost, because a predicate applied after the
fact still pays for every other syscall on the machine.

So a predicate naming one syscall is resolved to a number and written into the
instruction stream ahead of the map write, and
`the_syscall_filter_the_resolved_file_claims_is_the_one_that_ran` checks the
resolved file's claim against the run's own account of itself *and* against the
events — a run narrowed to `getppid` must contain no other syscall at all.

Only a conjunction is looked through. `syscall == 'read' || comm == 'sh'` names a
syscall and does not narrow to it: pushing that comparison into the program
would discard everything the other half of the `||` was about, and what came
back would be a subset nobody asked for. Such a predicate runs in userspace like
any other and the notes say so.

### A syscall name this build does not know is refused rather than matched

`syscall == 'wriet'` is a typo. It would push nothing down, match nothing in
userspace and report an empty series, which reads exactly like a process that
made no calls. It is refused at attach instead, naming the architecture whose
table this build carries.

That table is `src/collect/proc/syscall_names.rs`, data taken from
`asm/unistd_64.h` at the time it was written rather than at build time, because
a build that needed kernel headers is the dependency `docs/bpf-layer.md` says
this project does not have. **It is x86-64's and only x86-64's**: the numbers are
not portable — 1 is `write` here and `exit` on arm64 — so on any other
architecture `name()` answers nothing, the event reports `syscall_<n>`, and a
predicate written against a name is refused rather than silently matching
nothing. Adding an architecture is one array and one `cfg`.

### `sys_exit` hands over no task, so the name comes from a helper

Every other program in this layer reads `comm` out of a `task_struct` the
tracepoint gave it. `sys_exit(pt_regs *, long ret)` gives neither. The
alternative — a `/proc/<tid>/comm` read in userspace at drain time — answers for
whoever holds that tid *then*, and a short-lived task is exactly the one a
syscall histogram is interesting about. So the program calls
`bpf_get_current_comm`, once per row rather than once per call, and the request
to name that helper in the shared `insn::helper` module is in
`docs/open-findings.md`.

### A path is read with the helper that stops at the terminator

`linux_binprm.filename` is a `const char *`, and a fixed sixty-four-byte
`bpf_probe_read_kernel` over it walks past the end of whatever allocation holds
the name. `bpf_probe_read_kernel_str` stops at the NUL and zeroes the whole
destination on failure, which also satisfies the layer's rule that every byte of
a record is written before it is published. A path that fills the record is
reported as truncated on the event rather than silently shortened: a long path
and a short one are indistinguishable afterwards, and only one of them can be
matched against.

### What cannot be exercised here

**A cgroup or container target, for any of the four.** The catalog marks both
`bpf_only` on `proc.exec`, `proc.exit` and `proc.signals`, which is a statement
about which *backend* could serve them rather than a claim that this build does.
Narrowing a program to a cgroup needs a helper this layer does not have —
`bpf_get_current_cgroup_id`, or `bpf_current_task_under_cgroup` with a
`BPF_MAP_TYPE_CGROUP_ARRAY` — and the request is in `docs/open-findings.md`.
Until then the collectors **refuse both targets by name**, saying which helper is
missing and what to use instead. A target refused by name costs a reader one
message; a target accepted and quietly widened to the whole machine costs them a
wrong answer. The same refusal is what `sched.wakeup` has done since wave two.

**`proc.syscall`'s `req` and `file` keys.** The catalog declares
`keys: [task, req, file]` and every event carries `task` alone. That is the
shape of defect 5 — a key the catalog promises and the build does not emit — and
it belongs to whoever owns `catalog/*.yaml`; the request is in
`docs/open-findings.md`. The same is true of the `cgroup` key on `proc.exec` and
`proc.exit`, for the same missing helper as the cgroup target.

**The non-x86-64 half of the syscall table**, as above.

**A `bpf:tp` collector for any of the four.** Deliberate. The classic path needs
a numeric event id out of a root-only file under tracefs; the raw path needs a
name and the kernel's own BTF and reaches every one of these four tracepoints on
this kernel. Writing the classic one would be writing a second road nobody would
drive. What the chain is for now is `examples/proc-not-built.yml`.

## Needs from the shared layer

All of these are written up with the exact signature wanted under
`## Needs from another stream` in `docs/open-findings.md`. In short:

- **Two helper numbers in `src/bpf/insn.rs`** — `GET_CURRENT_COMM = 16` and
  `PROBE_READ_KERNEL_STR = 115`. Both are declared locally in this family's
  programs meanwhile, with the reason.
- **Three program fragments that are now copied three times** — the unrolled
  log2 bucket, the per-cpu drop counter increment, and the `probe_read_kernel`
  of one structure field. `runqlat.rs`, `offcpu.rs` and both of this family's
  programs each hold their own copy.
- **`map_create` should carry the capability advice on `EPERM`.** It is the
  first thing an unprivileged run hits, before any program is loaded, and a bare
  "Operation not permitted" names neither the capability nor the way to get it.
  This family wraps it locally; the layer should do it once.
- **A cgroup narrowing**, as above.
- **A catalog pass** on the two key declarations that nothing emits.
