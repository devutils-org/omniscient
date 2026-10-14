# `io`: how long reads and writes take, and at which layer

Lazy default: `omn io --cost med -- ./prog` (`io.syscall` histogram; `--cost
high` adds `io.latency`). The family has no `low` band.

## The layers of one read

A `read(2)` crosses layers, and each source times one of them. Picking the
layer is most of choosing the source:

```
application ── io.syscall ──┬── io.fs (VFS, regular files) ──┬── page cache ── io.pagecache / io.readahead
                            │                                └── block layer ── io.queue ── io.latency (device)
                            └── pipes, sockets, terminals (in io.syscall, not in io.fs)
```

| Question | Source | Mode |
|---|---|---|
| How long does the program wait on read/write, per descriptor? | `io.syscall` | `trace -e hist` |
| How long does the filesystem take, per file? | `io.fs` | `trace -e hist` or `topk` |
| How long does the device take, per request? | `io.latency` | `trace -e hist` |
| How long do requests wait in the software queue? | `io.queue` | `trace -e hist` |
| How long are fsyncs? | `io.fsync` | `trace -e hist` |
| Is it reading from cache or from disk? | `io.pagecache` | `trace -e summary` |
| Is readahead helping? | `io.readahead` | `trace` |
| How many bytes, how many operations hit the device? | `io.bytes`, `io.iops` | `count` or `poll` |
| What size are the requests? | `io.size` | `trace -e hist` |
| How busy, how deep is the device queue? | `io.util`, `io.inflight` | `poll@1s` |
| Every VFS call by kind (`vfsstat`) | `io.vfs` | `count` |
| Writeback by the kernel | `io.writeback` | `trace` |
| Is the machine stalled on I/O? | `io.pressure` | `poll@1s` |

## Examples

```console
omn trace io.syscall -m trace -e hist,summary -- ./prog
omn trace io.fs -m trace -e topk:10 -k file -- ./prog
omn trace io.latency -m trace -e hist -k dev,dir --system -d 30s
omn trace io.latency -m trace -e hist --filter 'bytes >= 65536' --pid 1234 -d 10s
omn trace io.fsync -m trace -e hist,summary -- ./db-bench
omn trace io.pagecache -m trace -e summary -k file -- ./prog
omn trace io.util,io.inflight -m poll@1s -e timeline --system -d 1m
omn trace io.bytes -m poll@1s -e timeline -k dev,dir --system -d 1m
```

## Targets

- `io.syscall`, `io.fs` and `io.fsync` take a launch, `--pid`, `--tid` natively.
- `io.latency`, `io.pagecache`, `io.readahead` and `io.vfs` take a process
  target **only through eBPF** (`bpf_only`): the program attributes each request
  to its submitter. Without the capabilities, only `--system` or `--cgroup`.
- `io.queue`, `io.merges`, `io.inflight`, `io.util`, `io.writeback` are the
  machine's (or a cgroup's): give the program as the window, `omn trace io.util
  --system -- ./prog`.

## Attribution: reads yes, writes mostly no

A read is issued by the task that asked for it, so `io.latency` attributes it
to the reader (`IDENTITY`). A write usually leaves through **writeback in a
kworker**, long after the `write(2)` returned: the device time is not the
writer's, and the source falls back to the file key (`file:dev+ino`, `KEY`).
`io.syscall` sees the write return fast; `io.fsync` is where a writer pays.

## Backends and cost

- `io.latency` with `-e hist` alone, in `trace`, no filter and no key but `dev`,
  is built **in the kernel**: one row per device and log2 bucket. That is the
  cheap way to get a device latency distribution on a busy machine.
- `io.fs`, `io.fsync` and `io.vfs` attach fentry trampolines (`high`); the
  attach alone costs about 65 ms, which is most of a short run's cost.
- Without eBPF, `io.latency` falls back to `/proc/diskstats`: per device, no
  latency, no submitter. The run says so.

## Traps

- **Cold and warm runs differ by an order of magnitude.** Decide which one you
  measure (drop caches, or warm up first) and do not mix them: a bimodal
  distribution gets mistaken for noise.
- `io.bytes` is what the **device** moved, not what the application asked for:
  readahead, metadata and journal writes are in it, cache hits are not.
- A bio-based device (`zram`) issues no request: none of its I/O is in
  `io.latency`.
- On btrfs over several devices one file's requests land on several
  partitions; a filter on one `dev` undercounts it.
- `io.util` at 1.0 is busy, not saturated, on a device that serves many
  requests at once (NVMe); read `queue` beside it.
- A function in the kernel's BTF is not always on the call path: a kernel
  built with `vfs_write` inlined into the syscall (some clang builds) never
  calls the copy a trampoline patches. The probe checks it with a call of its
  own, and `io.fs`, `io.fsync` and `io.vfs` are then refused with the reason
  rather than counting nothing; `io.syscall` still serves the syscall layer.
