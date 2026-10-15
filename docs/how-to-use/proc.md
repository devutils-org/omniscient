# `proc`: syscalls, process life, descriptors, threads, signals

Lazy default: `omn proc -- ./prog` (`proc.threads` and `proc.fd` polled).

## Which source for which question

| Question | Source | Mode |
|---|---|---|
| Which syscalls, how many, how long (`strace -c`)? | `proc.syscall` | `count -k syscall -e summary` |
| Every syscall with its return and path (`strace`)? | `proc.calls` | `trace -e raw` |
| What does it exec? | `proc.exec` | `trace -e raw` |
| How do its processes exit, with which code? | `proc.exit` | `trace -e raw` (`--system`) |
| How many descriptors, against which limit? | `proc.fd` | `poll@1s` |
| How many threads? | `proc.threads` | `poll@1s` |
| Every process of the machine, or every thread of one (`top`, `ps`) | `proc.tasks` | `poll@1s -e raw` |
| Which signals were sent, by whom? | `proc.signals` | `trace -e raw` |

## Examples

```console
omn trace proc.syscall -m count -k syscall -e summary -- ./prog
omn trace proc.syscall -m count -e topk:10@syscall --system -d 5s
omn trace proc.calls -m trace -e raw --filter "syscall == 'openat'" -- ./prog
omn trace proc.calls -m trace -e raw --filter 'errno != 0' -- ./prog
omn trace proc.exec -m trace -e raw -- make -j8
omn trace proc.exit -m trace -e raw --filter 'group_dead == 1' --system -d 30s
omn trace proc.fd,proc.threads -m poll@1s -e timeline --pid 1234 -d 1m
omn trace proc.tasks -m poll@1s -e raw --system -d 3s
```

## Targets

- `proc.syscall`, `proc.calls`, `proc.exec` take a launch, `--pid`, `--tid`,
  `--system`. `proc.exec` on a process keeps the execs of it and of its
  descendants.
- `proc.exit` and `proc.signals` (per process through eBPF) see the machine.
- `proc.fd` and `proc.threads` are one process's (launch or `--pid`).

## Cost

**`proc.syscall` and `proc.calls` are `extreme`**: their program runs on every
syscall of the machine (raw_syscalls fires on everything) and leaves early for
the ones not asked for. On a program making hundreds of thousands of calls a
second they dilate it, and `proc.calls`' ring fills and counts what it could
not publish. Narrow with a filter naming one syscall where you can; it is
written into the program.

`proc.syscall` aggregates in the kernel (one row per task, syscall and bucket),
so it is far cheaper than `proc.calls` for the same question. Use `proc.calls`
when you need each call.

## Traps

- A count of `proc.exit` events is a count of **threads**:
  `--filter 'group_dead == 1'` keeps one per process.
- A call already in progress at the attach is in no record; one still running
  at the end is said, not recorded.
- `proc.fd`'s count means nothing without the limit beside it (900 is fine
  against 524288 and an outage against 1024).
- Syscall names come from a table built for x86-64 and aarch64. On any other
  architecture the rows are numbered (`syscall_63`) and a filter by name is
  refused: filter on the number there.
