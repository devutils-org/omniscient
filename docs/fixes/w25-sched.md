# Stream sched, the twenty-fifth wave

Branch `feat/w25-sched`. Done sequentially by one agent, in the main checkout.

## Outside the owned files, said first

- `src/bpf/attach.rs` (shared): `cpu_clock`, a `BPF_PROG_TYPE_PERF_EVENT`
  program on every cpu's cpu-clock event at a frequency.
- `src/bpf/map.rs` (shared): `Counters::read_per_cpu`.
- `src/bpf/syscall.rs` (shared): `BPF_PROG_TYPE_PERF_EVENT`.
- `src/bpf/mod.rs`: `mod sched_runqlen` and its exports.
- `CATALOG.md`: a `sched.runqlen` row.

All three shared changes are additions; nothing that was there changed.

## What landed

1. **`sched.offcpu`'s provenance** (`named_by`, `perf_maps`, `executables`),
   kept for symbolizers retired during the run.
2. **`ppid`** on `sched.switches`, `sched.migrations`, `sched.wakeup`,
   `sched.lock`; a filter field on those and on `sched.offcpu`,
   `sched.runqlat`, `sched.futex`, `sched.slices`. The field checks leave
   `ppid` to the collector.
3. **`sched.lock`'s `addr` and `caller`**: the lock's address in the row key
   (an attribute on the event), a kernel stack per row and its first frame
   past the lock machinery.
4. **`sched.slices`' `wake_ns`**: wakeup and preemption stamps in an LRU map,
   closed at the switch in.
5. **`sched.runqlen`**: a per-cpu run queue length histogram on a 99 Hz
   cpu clock.

## Shown vs reasoned

- `ppid` (shown by unit test on this process's own row).
- Everything eBPF (reasoned): each program builds against this kernel's BTF
  (7.2: `cfs_rq.nr_queued`/`nr_running` located, `contention_*`,
  `sched_wakeup*`) and an instruction-order test pins what it does where. No
  CAP_BPF here.

## Privileged runs wanted

- `sched.runqlen`: `omn trace sched.runqlen -m sample -e hist --system -d 5s`
  under `stress-ng --cpu 48 -t 6s` against `runqlen.bt` (bpftrace) for the
  same seconds: most ticks at length 1 on a 24-cpu machine, few at 0.
- `sched.slices`' `wake_ns` against `perf sched timehist`'s `sch delay` for a
  program that sleeps 2 ms 100 times: means within a few microseconds.
- `sched.lock`'s `caller`: a pipe-writing contender
  (`omn trace sched.lock -e topk:5@caller --system -d 3s`) against `perf lock
  contention -a -- sleep 3`'s caller column (e.g. `pipe_write`).
- `sched.offcpu -e stack --format json`'s `named_by` and `executables` on a
  Python program, beside `cpu.profile`'s.

## Not done

Nothing of the stream's list.

## Tests

`--lib collect::sched switch_count sched_lock sched_runqlen`, `--test
family_sched --test scenario_sched --test catalog_model --test cli --test
lazy`, clippy, fmt. On this machine, as on `main`:
`family_sched::a_counted_switch_rate_is_per_second_of_the_run` and
`scenario_sched::a_source_with_no_collector_is_refused_with_its_own_exit_code`
fail for want of CAP_PERFMON and CAP_BPF. The shared layers changed by
addition only; the coordinator's full run covers them.

## Decision lines for `DECISIONS.md`

- A kernel address that names an object for its lifetime only (a lock) is an
  attribute, never a key.
- `ppid` is put on an event by its collector, from the program's fork records
  or `/proc`, and is declared a filter field wherever an event carries it.
