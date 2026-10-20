# The eBPF layer

Sixty-two of the catalog's hundred and fifty-five backends are eBPF. This is
the layer they share: how a program is built, how it is loaded, how its events
come back, and how what it lost is counted. `io`, `net`, `proc`, `mem` and
`runtime` all wait on it, so it is written to be called rather than copied.

It lives in `src/bpf/` and is shared. A family does not edit it; it asks for
what it needs under `## Needs from the shared layer` in its own
`docs/families/<family>.md`.

## What it is, and what it deliberately is not

There is no clang, no `vmlinux.h`, no object file and no build step. A program
is an array of instructions built in Rust, and the kernel field offsets it
needs are read from `/sys/kernel/btf/vmlinux` at load time and written into the
instruction stream before it is handed to the verifier.

That is the same relocation a compile-time CO-RE build performs, moved a few
milliseconds later. What it buys:

- `omn` stays one binary. Building it needs a Rust toolchain and nothing else —
  no clang, no bpftool, no kernel headers on the build machine.
- The offsets cannot be stale, because there is no earlier moment at which they
  could have been baked in. The kernel that is running is the one that answers.
- Invariant 8 holds: this talks to `bpf()`, not to `libbpf`'s idea of it.

What it costs, stated plainly: a program is written as instructions, and a
mistake comes back as a verifier log rather than as a compiler error. The log
travels with the error for exactly that reason — `LoadError` carries it, and
the collector prints it.

## The pieces

| File | What it does |
|---|---|
| `syscall.rs` | `bpf()` itself: `MAP_CREATE`, `PROG_LOAD`, `RAW_TRACEPOINT_OPEN`, `MAP_LOOKUP_ELEM`. One `#[repr(C)]` struct per command, each passing its own size. |
| `insn.rs` | The instruction encoding, and `Program`, which resolves jumps by label so inserting a line cannot silently break a jump that crosses it. |
| `btf.rs` | The running kernel's types. Answers two questions: where is this field, and does this type exist. |
| `ring.rs` | The `BPF_MAP_TYPE_RINGBUF` consumer: read the published records, then publish the consumer position once, after the batch is out. |
| `map.rs` | Maps shared with a program: a hash map to count in, a per-cpu array to count losses in, and the walk that drains one. |
| `wakeup.rs` | The first program: `sched_waking`, one record per wakeup. Read it as the worked example for streaming. |
| `offcpu.rs` | The first aggregating program: `sched_switch`, counting nanoseconds per task in a hash map. Read it as the worked example for counting. |

## Writing a program

```rust
let btf = crate::bpf::kernel_btf()?;                 // read once, kept
let pid_offset = btf.member_offset("task_struct", "pid")?;

let mut program = Program::new();
program.push(Insn::mov64_reg(6, 1));                 // keep the context in r6
program.push(Insn::ldx(BPF_DW, 7, 6, 0));            // first tracepoint argument
program.push(Insn::call(helper::KTIME_GET_NS));
program.push(Insn::stx(BPF_DW, R10, EVENT, 0));      // ... into the record
program.jump_to("done", Insn::jsge_imm(0, 0, 0));    // jumps name a label
program.label("done");
program.push(Insn::exit());
let instructions = program.build();
```

Three rules that are not style:

1. **`r6` through `r9` survive a helper call; `r1` through `r5` and `r0` do
   not.** Anything needed after a call lives in the first group.
2. **Every byte of a record is written before it is published**, padding
   included. An uninitialised padding word is whatever was on the stack.
3. **`bpf_ktime_get_ns()` only.** `bpf_ktime_get_boot_ns()` includes suspend
   time. Mixing the two puts the stream in a different clock domain from every
   other collector and destroys every join with no error anywhere.

## Whose events a program keeps

Every program that narrows to a target asks one question, in one place:
`insn::Whose` is the whole machine, a process (`tgid`), a thread (`pid`, the
kernel's thread id) or a cgroup, and two instruction sequences in
`src/bpf/insn.rs` compare it. Before the thirteenth wave each program wrote
its own comparison against the thread group alone, so a `--tid` target kept
nothing for any thread but a process's first, and every program refused
`--cgroup`.

- `push_current_filter` when the subject is the current task -- a syscall, a
  lock wait, a signal's sender. One helper call.
- `push_task_filter` when the subject is another task -- `sched_switch`'s
  incoming task, `sched_waking`'s wakee, the task being migrated -- read
  through the `TaskFields` offsets BTF gives. Keep the task pointer in `r6` to
  `r9`: the reads are helper calls.

A cgroup means **that cgroup and everything below it**, compared the way the
kernel's `cgroup_is_descendant` does: by the target's level in the hierarchy
and the task's ancestor at that level (`bpf_get_current_ancestor_cgroup_id`
for the current task, `task->cgroups->dfl_cgrp`'s `ancestors` otherwise, in
both the 6.1-and-later and the older layout). The two simpler helpers are not
used: one sees only a task's own cgroup, and a `--cgroup user.slice` would miss
every session below it; the other answers only for the current task. A pointer
that reads as null along the walk skips the event: a filter that keeps what it
cannot judge is not a filter. Inside a cgroup namespace the depth read from the
path is wrong and nothing a process can read corrects it, so that case is not
served.

## Streaming or counting

Two shapes, and the choice is about volume rather than taste.

A **streaming** program writes one record per event into a ring buffer and
userspace reads them. It is the right shape when each event matters on its own
-- a wakeup is a pair of task ids that a join needs individually, and there is
nothing to add up. `wakeup.rs` is that shape.

A **counting** program keeps the arithmetic in the kernel: a hash map keyed by
whatever the series is keyed by, updated in place, drained by userspace on a
tick. It is the only viable shape when the event rate is a property of the
workload rather than of the question. `sched_switch` fires a few million times
a minute on a busy machine; streaming it makes the cost of the measurement grow
with the thing being measured, and the numbers then describe a machine running
`omn`. `offcpu.rs` is that shape, and it turns those millions into a few
hundred map entries.

Three things about counting are not obvious, and each one is a wrong number
rather than an error:

1. **The accumulation has to be atomic.** A `BPF_MAP_TYPE_HASH` value is one
   allocation shared by every cpu. A load, an add and a store loses whichever
   increment lands between the first two, and loses more of them the busier the
   machine is -- which is to say, most where the number matters most.
   `Insn::atomic_add` is the increment; the per-cpu maps are the only ones that
   do not need it.
2. **Reading and removing must be one operation.** A drain that looks an entry
   up and then deletes it loses whatever the program added in between.
   `map_take` is `BPF_MAP_LOOKUP_AND_DELETE_ELEM`, which hash maps have served
   since 5.14, below the kernel floor.
3. **A drain is not a snapshot.** There is no cursor and no atomic iteration:
   the walk asks for the key after this one until there are none, while the
   program keeps writing. So a drain takes what it finds and leaves the rest
   for the next pass, each drain is a delta over the interval between two of
   them, and the interval belongs on the event -- a number aggregated over a
   window that does not say which window cannot be compared with another.

A map is bounded, and a full one is a loss like any other: the update helper
returns an error, the event is not counted, and only the program's own drop
counter records that it happened.

## Attaching

Through `BPF_RAW_TRACEPOINT_OPEN`, which takes a tracepoint's name.

The classic path reads a numeric event id out of `/sys/kernel/tracing`, which
is mounted and unreadable by a normal user on a normal machine — including this
project's development machine, where `/sys/kernel/tracing/events/sched` is
`Permission denied` at uid 1000. A raw tracepoint needs BTF and a name. So
`bpf:raw_tp` is ordered ahead of `bpf:tp` in every chain, and `bpf:tp` declares
`equivalent: true`: it sees the same events, it just demands more to attach.

A raw tracepoint hands its program an array of `u64`, one per argument, and
nothing in that array says what any of them is. A program reads slot two as a
`task_struct *` because the tracepoint's prototype says so -- and prototypes
change: `sched_switch` gained a fourth argument in 5.18, and a program written
for the older shape reads a pointer out of a slot holding an integer and gets
a plausible number back. So the prototype is checked before the program is
built. The kernel publishes it as `typedef void (*btf_trace_<name>)(void *,
args...)`, and `Btf::tracepoint_signature` returns the arguments after the
leading data pointer, which `offcpu.rs` compares against the shape it was
written for and refuses by name when they differ.

Availability is probed without attaching anything, and without privileges:
every tracepoint the kernel defines carries a `btf_trace_<name>` type in its own
BTF, so `raw_tracepoint:sched_waking` is answered by looking for
`btf_trace_sched_waking`. The requirement string is `raw_tracepoint:<name>`.

## Losses

The BPF ring buffer does not report its own overflows. When a program cannot
reserve space, `bpf_ringbuf_output` returns an error, the event is gone, and
nothing anywhere records that it existed.

So **every program in this layer carries a drop counter** — a one-entry
`BPF_MAP_TYPE_PERCPU_ARRAY`, incremented when the output fails, per-cpu so the
fast path takes no atomic — and the collector reads it into `Losses::dropped`
before the map is closed. A correlation missing a third of its evidence must
not look like a correlation that found nothing.

Records the reader does not recognise are counted as `undecoded` rather than
reinterpreted.

**A preempted program skips every run behind it on that cpu.** A
raw-tracepoint program runs with migration disabled, not preemption, and the
kernel's per-cpu recursion guard (`prog->active`) stays raised while a
preempted run waits. Every later run of the same program on that cpu is
skipped, and counted in the program's `recursion_misses` (which the collector
reads as `fentry::skipped` and reports as a loss), until the preempted one
resumes -- a whole timeslice of events, not one. Measured in the fourteenth
wave with `io.readahead` beside 48 processes re-reading a cached file on 24
cpus: 377 000 to 478 000 skipped runs in a two-second session, and the
workload's own lookups short by 3 to 15 of 256. Nothing was silent, but the
count is every task's, not the target's, so it is never slack for one
workload's count (`DECISIONS.md`, the fourteenth wave). Narrowing earlier does
not stop the skips, since the guard is taken before the program runs, but it
does keep the target's own runs short, which is why a process target lost none
of its lookups below; a shorter program does not help (below).

**Measured again on 7.2 (2026-10-20), and the program cannot close it.**
Upstream's `__bpf_trace_run` takes the guard under
`rcu_read_lock_dont_migrate()` and the tracepoint under SRCU-fast, so the run
is preemptible from the guard to its release. It is preemption: with
`io.readahead` system-wide for two seconds beside readers of a cached file on
24 cpus, 4 and 12 readers cost 0 to 100 skipped runs, 48 readers 144 000 to
205 000. Disabling preemption inside the program (`bpf_preempt_disable` after
the context is saved, `bpf_preempt_enable` before each exit; every program
of `omn selftest` loaded so, 199 of 199) changed nothing: 187 000 to 205 000
skipped either way, three alternated runs each. A reschedule asked for while
preemption is off is taken at the `enable`, which is still inside the guard,
so the guard is held across the preemption all the same. Only the kernel can
disable preemption from the guard to its release; a program of its own can
only move where it is preempted.

**Whose the skipped runs are, measured on 2026-10-20.** A process target
reading a cached 32 MiB file four times in 4 KiB `pread`s, beside 48 readers
of another file on 24 cpus: every one of its 32 768 lookups was kept, in each
of three runs, while the programs reported 3 000 to 4 600 skipped runs. The
skips were the other readers', which leave at the narrowing, and the run still
said its totals were lower bounds. Telling the two apart was tried with a
witness: a program of a few instructions on each tracepoint, attached before
the real one, narrowing to the target and counting, beside a count of the
target's runs in the real programs. The two counts agreed to the run (32 784
and 32 784), but the witness was skipped 820 to 6 566 times itself, more than
the program it witnessed (1 286 to 2 548), so a difference of zero could not be
read as no loss. The guard is held from the dispatch to the release, and the
dispatch costs as much as a short program: **a shorter program does not narrow
the window**, and no program of this layer can say whose a skipped run was.
The experiment was not kept. What is left is the kernel's: a guard taken with
preemption off, or `recursion_misses` per task.

## Privileges

`CAP_BPF` and `CAP_PERFMON`, as settled in `DECISIONS.md`. Root works because
root has them. An unprivileged run is refused, and the refusal names both
capabilities and the `setcap` line that grants them; it is never served by a
lesser backend that answers a different question.

The probe for the `bpf` requirement is a load, not a guess: the smallest
program the verifier accepts, of the same type and privilege class as the real
ones. Reading `unprivileged_bpf_disabled` and the effective capability set would
be reasoning about the kernel's answer instead of asking for it.

```console
$ sudo setcap cap_bpf,cap_perfmon+ep target/debug/omn
```

## Debugging a rejection

`LoadError` carries the verifier's log, at `log_level` 1: the instruction that
failed and the state around it. Level 2 prints every instruction the verifier
walked and is megabytes for anything real.

The four rejections worth recognising on sight, none of which say what a
compiler would say:

- **`R1 type=inv expected=fp`** — a helper wants a stack pointer and was handed
  something else, usually because a helper call clobbered `r1`.
- **`invalid indirect read from stack`** — a byte of the record was never
  written. Look for padding.
- **`math between fp pointer and register`** — an offset built at run time from
  a value the verifier cannot bound.
- **`unknown opcode`** — the encoding is wrong, which means `insn.rs`, not the
  program.

## What is not here yet

Named because a gap that is not written down gets rediscovered by each family
in turn:

- **Per-cpu ring consumption in parallel.** One ring, drained on the tick loop,
  is enough at the volumes seen so far. A syscall tracer will not agree.

Four things this section used to list are here now, and are worth knowing about
rather than rediscovering:

- **fentry.** BTF-typed attach, landed in the eighth wave: `src/bpf/fentry.rs`
  loads a `BPF_PROG_TYPE_TRACING` program with `expected_attach_type` and the
  function's `attach_btf_id`, and attaches it through `BPF_RAW_TRACEPOINT_OPEN`
  with no name. The requirement class is `fentry:<function>`: the running
  kernel's BTF declares a `FUNC` of that name, and, where this process may
  load a tracing program, the smallest one loads against it.
  `attach::probe_type_support` answers it, beside `kprobe` and `uprobe`.
- **USDT.** Also the eighth wave: `src/bpf/usdt.rs` reads a target's
  `.note.stapsdt` notes for each probe's location, semaphore and argument
  descriptors, and attaches a uprobe at the right offset. `runtime`, the
  `net.request` collector and `sched.mutex`'s glibc backend use it.

- **Stack capture.** `bpf_get_stackid` into a `BPF_MAP_TYPE_STACK_TRACE`, with
  the id in the *key* of the totals map so that two blocks of the same task from
  different call sites are different entries. `src/bpf/offcpu.rs`.
- **A log2 bucket in an aggregation key.** An unrolled six-step binary search,
  thirty instructions and no loop; `LOG2_STEPS` in `src/bpf/runqlat.rs` is the
  table. Counting `(key, bucket) -> n` is what makes `--emit hist` possible on
  a source that never hands up one event per measurement -- and the emit layer
  reads `emit::WEIGHT_ATTR` off each row so a bucket standing for four hundred
  waits counts as four hundred.
