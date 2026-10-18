# Stream docs, the twenty-fifth wave

Branch `feat/w25-docs`. Done sequentially by one agent, in the main checkout.

## What landed

- **`docs/triage.md`**: the answers' `failures` (and the exit 1 beside them)
  and `program_stderr`; `followed.job.under`, `unplaced`, `unplaced_on_cpu`;
  `omn saturate`'s `failures`.
- **`docs/cases.md`**, "The agent's reading": the thread blocked on a
  connection named beside the block, the job by `ppid`, `named_by` read
  before the perf maps, the tool's own eBPF samples set aside.
- **`docs/unwind.md`**: the exec re-read (`Cfi::reread_for`) and
  `CALLTREE`'s pad, with the numbers that justified each.
- **`docs/view/system.md`, `process.md`, `request.md`**: the machine's
  process list, the `dir` split, a recording's capacity, the request's
  bytes read and queued (the stale "bytes on the network" gone).
- **`GUIDE.md`**: "The shape of a run" -- a program alone, a target beside a
  program, several sources over one window, `-r N`, and the three exit
  codes.
- **`docs/bench.md`**: what `io.fsync`'s share was (the trampolines' unlink,
  after the window since the twenty-fourth wave), and `net.tcp.connect`'s
  the same, with the `bpf:raw_tp` road beside it.
- **`docs/how-to-use/`**: the wave's new sources and keys in each family's
  table (`mem.oom -m trace`, `mem.slab`, `--keys mapping`, the heap's stacks
  through uprobes, `net.snmp -k counter`, `net.sockstat`, `sched.runqlen`,
  `power.wakeups --system`, `power.process_energy`, `power.cstate -m count`,
  `proc.calls`' exec paths).

## Tests

`cargo test --test cli` (the index and session tests).
