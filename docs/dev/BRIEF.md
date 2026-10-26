# Brief — the kernels without BTF: the classic tracepoints, through a perf event array

The twenty-seventh wave: **serve the two root cells the twenty-sixth wave left
structural** (`docs/distroproof.md`, "After the twenty-sixth wave's
follow-up"). Debian 10 runs 4.19, with no kernel BTF; Ubuntu 20.04 runs 5.4,
whose BTF declares no `btf_trace_*` types, so no raw tracepoint can be checked
or attached by type. Neither has the BPF ring buffer (5.8). As root, both load
eBPF programs; every source whose only viable backend there was `bpf:tp` read
"out of reach" for the build's reason, because no collector was written for
the classic road.

Run autonomously on 2026-10-26, after the user's *"va aussi loin que tu
peux"*, in one worktree (`omniscient-claude`, branch `claude/w27`), one stream
after another, each verified under `distroproof run` on both kernels before
its commit. Not launched as parallel streams: each step needed the one before
it.

## What it built

1. **The transport** (`src/bpf/transport.rs`): the ring buffer where the
   kernel creates one, a `BPF_MAP_TYPE_PERF_EVENT_ARRAY` with one mmap'd ring
   per cpu otherwise, merged by the record's stamp at the drain.
2. **The classic record** (`context::Record`): a tracepoint's fields at the
   offsets `format` gives, read in place where aligned and through a probe
   read otherwise; `__data_loc` strings.
3. **The classic follower** (`sched_common::Descendants`): `task_newtask` and
   `sched_process_free`, with a threads map, where the raw pair is missing.
4. **The old kernels' walls**: `bpf_probe_read` in place of the `_kernel` and
   `_user` read helpers before 5.5, `LOOKUP_AND_DELETE` on a hash before 5.14,
   the memlock limit raised whenever a large map is refused, and a load
   retried without a verifier log when 4.19's log fills.
5. **The collectors**, by family: `proc` (calls, exec, exit, signals,
   syscall), `sched` (wakeup, runqlat, offcpu with and without stacks, futex,
   migrations, switches, irq, numa_balance), `io` (queue, size, latency,
   writeback, syscall), `mem` (kmalloc, page_alloc, reclaim, compaction),
   `net` (softirq, drops, retransmit, lifetime, connect), `power.cstate`,
   `cpu.oncpu_time`.
6. **The catalog**: each `bpf:tp` row equivalent or degraded by what its
   record carries (`docs/DECISIONS.md`, "Settled in the twenty-seventh wave"),
   the `follower` requirement, and the ring-buffer requirements no program had.

## What it measured

Usable sources, of 123, as root under `distroproof run`: **Debian 10, 58 to
74; Ubuntu 20.04, 64 to 86**. On this machine, 186 of 244 backends have a
collector (152 of 242 before). Ground truth on both kernels: `net.softirq`'s
runs are `/proc/softirqs`' to the run (72 of 72, 79 of 79); `cpu.oncpu_time`
holds a launched spin to its reap within 20 us; `io.syscall` counts `dd`'s 300
records as 606 calls; `net.tcp.connect` sees 3 established and 2 refused, as
sent; `net.tcp.retransmit` sees the resends netem forced.

## Left, and why

`net.qdisc`, `sched.lock`'s tracepoints, `io.pagecache` and `io.readahead`
need tracepoints these kernels do not define; `sched.slices` reads the
kernel's `sched_info` stamps, which only BTF reaches; `power.freq` and
`power.throttle` have nothing to be checked against in a guest. The matrix
sweep of every cell with this wave's binary is `docs/dev/STATE.md`'s first
item after it.
