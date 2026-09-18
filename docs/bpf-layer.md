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
| `wakeup.rs` | The first program: `sched_waking`, one record per wakeup. Read it as the worked example. |

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

## Attaching

Through `BPF_RAW_TRACEPOINT_OPEN`, which takes a tracepoint's name.

The classic path reads a numeric event id out of `/sys/kernel/tracing`, which
is mounted and unreadable by a normal user on a normal machine — including this
project's development machine, where `/sys/kernel/tracing/events/sched` is
`Permission denied` at uid 1000. A raw tracepoint needs BTF and a name. So
`bpf:raw_tp` is ordered ahead of `bpf:tp` in every chain, and `bpf:tp` declares
`equivalent: true`: it sees the same events, it just demands more to attach.

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

- **kprobes, uprobes, fentry and USDT.** Only raw tracepoints attach today.
  `io`, `net` and `runtime` need the others.
- **In-kernel aggregation.** `sched.offcpu` with stacks must count in a hash
  map keyed by stack id rather than stream raw events; that needs
  `BPF_MAP_TYPE_HASH`, `bpf_map_update_elem` and a way to walk a map from
  userspace.
- **Stack capture.** `bpf_get_stackid` and a stack map, which is groundwork G6
  and lands with symbolization.
- **Per-cpu ring consumption in parallel.** One ring, drained on the tick loop,
  is enough at the volumes seen so far. A syscall tracer will not agree.
