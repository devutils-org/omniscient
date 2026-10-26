# w26 kernel — what every eBPF collector did alone, in two modules

Phase 0b of the twenty-sixth wave (`docs/dev/BRIEF.md`, "0b · `kernel`").

## Landed

- **`src/bpf/program.rs`**, what every program did the same way: the
  license (23 copies, now one), the raw tracepoint load and its permission
  advice (8 identical wrappers gone, the generic `load` that `attach.rs` had),
  the raw attach and its refusal (39 call sites), **one prototype check**
  (`prototype`, seven local checkers gone: it returns which accepted shape
  the kernel declares and always names them all), and `skipped`, the
  kernel's skipped runs, which raw tracepoint programs reached as
  `attach::fentry::skipped`.
- **`src/collect/kernel.rs`**, what every eBPF collector did with a program's
  output: `KernelCounts` + `Lost` (cumulative loss counters to losses, each
  growth counted and said once; `Lost::skipped_runs` the one sentence),
  `UserFilter` (a predicate applied after delivery and its kept/removed
  note, in occurrences), `Drains` + `drain_rows` (a program whose rows are
  taken whole; the walk's vanished and truncated rows as losses). Tested on
  fabricated counters and rows: nothing loaded, no capability needed.
- **Pilots on it**: `sched.runqlat` (`RunqLatProgram` implements `Drains`;
  the collector's drain, filter and counters go through the module) and the
  block collector (`io.latency`, `io.queue`, `io.size`: counters and filter).
  The block ring's full count was noted once per drain whatever it grew by;
  through `KernelCounts` it is counted by what it grew.
- **The context as an adapter, where it is one**: `src/bpf/context.rs`,
  `Syscall { Raw(&Btf), Classic {..} }` taken out of `proc_calls`, with the
  `sys_enter`/`sys_exit` shapes. One program body serves `bpf:raw_tp` and
  `bpf:tp` on the syscall tracepoints; `proc.syscall` and `sched.futex`
  declare `bpf:tp` rows on the same two tracepoints, and phase 1 writes them
  through it.
- **Every program prototype held to the catalog in one test**
  (`bpf::tests::every_program_prototype_is_the_catalog_s`, 40 constants of
  24 programs, both directions). Five programs were held before.
- The five `#[path]` mounts in `attach.rs` are plain modules of `bpf`.

## A finding that changes the brief

**The context does not make the 38 `bpf:tp` rows nearly free.** A classic
tracepoint hands over a record the kernel filled with copied fields, not the
raw arguments: `sched_switch`'s record has `prev_pid` and `prev_state`, not
the `task_struct *` that `runqlat` reads its `sched_info` from;
`block_rq_issue`'s has `dev` and `sector`, not the `request *` that
`io.latency` keys on. For a tracepoint whose arguments are pointers, the
classic program reads other things and is another program -- the brief's
two pilots are both of that kind, so neither got a `bpf:tp` row. What the
classic path does buy is **no BTF**: a program on copied fields needs none,
which is what a pre-BTF kernel run as root lacks. The tracepoints whose
arguments are numbers (`sys_enter`/`sys_exit`, `cpu_frequency`, `cpu_idle`,
`softirq_entry`) are the ones a context adapter serves.

## Shown

- `cargo test --lib`: 1818 passed (`bpf` 449, `collect::kernel` 4,
  `collect::sched::runqlat` 9, `collect::io::block` 15).
- `tests/family_sched.rs` (25) and `tests/family_proc.rs` (7) through the
  binary with the capabilities, after the shared load and attach.
- `resolve::tests::a_backend_whose_requirements_cannot_be_checked_is_reported_as_unverified`
  moved to a catalog entry on `xdp`: `rdt:mbm` is answered since `requires`.

## Counts (`grep` over `src/`, the two modules excluded)

| | before | after |
|---|---|---|
| `const LICENSE` | 23 | 0 (two re-exports) |
| local raw tracepoint `fn load` wrappers | 9 | 0 |
| local prototype checkers | 7 | 0 |
| `raw_tracepoint_open(` outside `program.rs` | 50 | 10 (custom refusals) |
| `#[path]` mounts in `bpf/attach.rs` | 5 | 0 |
| `fn absorb_drops` / `filtered_out +=` | 21 / 32 | 21 / 30 |

The last line is the phase 1 work: each family moves its collectors onto
`KernelCounts`, `UserFilter` and `Drains` as runqlat and block did.
