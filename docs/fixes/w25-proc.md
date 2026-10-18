# Stream proc, the twenty-fifth wave

Branch `feat/w25-proc`. Done sequentially by one agent, in the main checkout.

## Outside the owned files, said first

- `src/bpf/mod.rs`: `pub use sched_common::Descendants`, so a collector
  outside sched follows a target's forks.
- `CATALOG.md`: the `proc.calls` row.

## What landed

1. **`proc.calls` reads the path of every open and exec**: `open`, `openat`,
   `openat2`, `creat` at the return; `execve`, `execveat` at the entry, kept
   in the pending entry (160 bytes now), since the return replaces the
   memory the path was in. Six calls by this machine's numbers, the ones an
   architecture lacks left out.
2. **`proc.calls` on `bpf:tp`**: the classic `raw_syscalls` tracepoints, a
   `Context` that reads the number, arguments and return at tracefs's
   offsets.
3. **`proc.exec`'s tree exact through `sched_process_fork`**: the root's
   forks followed in the kernel and adopted at each drain; the `/proc` walk
   remains behind it, noted when the forks are not followed, and a full map
   is a counted loss.

## Shown vs reasoned

All three reasoned: the programs build against this kernel (BTF readable),
the instruction tests check which reads happen where, and the tree's
adoption is unit tested. This machine grants no CAP_BPF and tracefs is
root's.

## Privileged runs wanted

- Paths: `omn trace proc.calls -e raw --filter 'syscall == "execve"' -- sh -c '/bin/true; /usr/bin/env true'`
  shows `path` `/bin/true` and `/usr/bin/env` (and `fd` on none); with
  `syscall == "openat2"` against a program using `openat2`, or `strace -f -e
  trace=open,openat,openat2,execve` for the same command.
- `bpf:tp`: `omn trace proc.calls -e summary -k syscall --backend force:bpf:tp -- ls`
  against `--backend force:bpf:raw_tp` (same counts).
- The exact tree: `omn trace proc.exec -e raw -- sh -c '(sleep 0.05; /bin/true) & wait'`
  keeps `/bin/true`, whose parent subshell never exec'd and exits before the
  drain; `failed_reads` stays 0.

## Not done

Nothing of the stream's list (`proc.tasks` in lazy's `low` band was done by
lazy).

## Tests

`--lib proc_calls collect::proc`, `--test family_proc --test scenario_proc
--test cli --test catalog_model --test lazy`, clippy, fmt.
