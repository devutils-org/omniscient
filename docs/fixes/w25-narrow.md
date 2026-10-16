# Stream narrow, the twenty-fifth wave

Branch `feat/w25-narrow`. Done sequentially by one agent.

## Said first: `feat(fentry)`, a shared layer

`src/bpf/fentry.rs`: `entry_instructions` takes `descendants: Option<i32>`,
the map fd of `sched_common::Descendants`; with a process target it stores
the thread group at `-24`, compares the root first, and looks every other
thread group up in the map, skipping on a miss (the shape of
`io_fsync.rs`'s gate). `HistogramProgram` holds the `Descendants`, built in
`load` for every process target; `missed()` adds its programs' skipped runs
and `unfollowed()` is new. The `Narrowing` struct itself is unchanged.

Call sites: `VfsProgram` (`io.syscall`, `io.fs`) and `SocketProgram`
(`net.socket`) pass `unfollowed` up; `collect::io::histogram::Rows` gains
`unfollowed()` (default 0) and the shared histogram collector says it once,
as `io.fsync` does.

## What landed

- `io.syscall`, `io.fs` and `net.socket` follow the processes a process
  target starts, and the ones it already has when the run attaches.
- Their caveats say so (`catalog/io.yaml`, `catalog/net.yaml`).

## Shown vs reasoned

- **Not attached here.** This machine grants no CAP_BPF, so no program was
  loaded. Shown: the emitted entry program (a unit test) has one more map
  lookup with a descendants map than without, compares the root as a
  constant first, and a machine target looks nothing up. The verifier was
  not run.
- Wanted, privileged (the exit criterion of item 1):

  ```console
  make   # with capabilities
  printf '#!/bin/sh\ndd if=/dev/zero of=/tmp/w bs=4k count=64 status=none\ndd if=/tmp/w of=/dev/null bs=4k status=none\n' > /tmp/io.sh && chmod +x /tmp/io.sh
  ./target/debug/omn trace io.syscall -m trace -e summary -- /tmp/io.sh
  ./target/debug/omn io -- /tmp/io.sh
  ```

  `io.syscall`'s summary should count about 131 calls (the two `dd`s'
  reads and writes plus the shell's), against n=9 before; `strace -f -c -e
  trace=read,write /tmp/io.sh` is the reference.

## Not done

- `net.tcp.accept` (`AcceptProgram`, not a `HistogramProgram`) still keeps
  one process: stream net's item.

## Tests

`--lib fentry` (1 new), `--lib collect::io`, `--lib collect::net`, `--lib
bpf::`, `--test family_io --test scenario_io --test family_net`, clippy,
fmt. `--test scenario_net`: two tests red here,
`a_net_source_this_build_does_not_collect_exits_three_and_names_the_backend`
and `the_three_answers_an_ebpf_net_request_can_be_refused_with`: they pin
`bpf:kprobe` and expect exit 3, and an unprivileged machine answers exit 1
first. Not this change; filed for net.
