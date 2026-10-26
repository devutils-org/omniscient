# The eighteenth wave, stream io

Branch `feat/w18-io`, from `main` at `9cd5409`. The items of `docs/dev/BRIEF.md`,
"io": the program statistics that read 0, which way the 348 ns points, udev
and the `chmod`, the polled queue the CI cannot see, and item 11's io lines.
The account of each is in `docs/scenarios/io.md`, "The eighteenth wave",
defects 20 to 22 and "19, continued".

## What landed

- **`src/bpf/io.rs`, `src/collect/io/block.rs`: the block programs stay
  loaded after the stop.** The run reads each collection's eBPF run time
  (`run_time_ns`, `run_cnt` in `/proc/self/fdinfo`) in `Spending::close`,
  after every collector has stopped, and nothing reads it earlier unless a
  `--budget` is set. `BlockCollector::stop` ended with `self.program = None`,
  so the descriptors were closed by then and the meter kept its first reading,
  zero: the *"programs ran 0 times for 0 ns"* beside 26 838 511 counted
  requests. `BlockProgram::detach` now drops the two links and keeps the
  programs; the stop calls it instead of dropping them. They run no more, and
  their account is readable until the collector is dropped, after the close.
- **`src/collect/io/diskstats.rs`: `io.inflight` is the larger of the
  kernel's two in-flight accounts.** `/proc/diskstats`' ninth column read 0 on
  the CI's `nvme0n1` at all 101 polls while `/sys/block/nvme0n1/inflight` read
  up to 199. The gauge now reads `/sys/block/<dev>/inflight` beside the
  diskstats line for each device it reports, takes the larger (each is a count
  of requests really outstanding, so the larger is never more than the device
  held), and carries both as `diskstats_inflight` and `sysfs_inflight` (absent
  when the file is not readable, not zero, and the device named in the run's
  notes). A workaround, not the cause: why 6.17's disk line read 0 stays open
  (below). `catalog/io.yaml`'s `io.inflight` says so in a caveat and a
  comment on its backend; `/sys/block` is not in `requires`, because the gauge
  works, and says what it lost, without it.
- **`tests/scenario_io.rs`**: the cost test's doc comment and its panic say
  `udevadm settle` before the `chmod` of `/dev/nullb0`, and why; and a new
  ignored test, `a_device_with_a_known_latency_puts_every_request_in_its_bucket_in_both_histograms`,
  on `null_blk` with `irqmode=2 completion_nsec=3000000`: 64 direct reads by
  `dd`, every streamed interval at least the configured latency and inside its
  log2 bucket, and both histograms (in-kernel and streamed) exactly one bucket
  holding all 64. The latency is read from the module's own parameters, and a
  value within a quarter bucket of an edge is refused.
- **`tests/scenario_io.rs`, the queue scenario**: a floor independent of
  `/sys/block/<dev>/inflight`, which the tool now reads too. At each poll's
  instant, the workload's own clock counts the direct writes begun and not
  returned; at ten polls or more with half the writers inside their syscalls,
  the gauge must read a queue. Here 101 of 101 polls, maximum 25. And the
  known-latency test asserts the in-kernel request said "the histogram was
  built in the kernel" and the streamed one did not, as the test it copies
  does.
- **`docs/scenarios/io.md`**: defects 20 (the queue), 21 (the statistics) and
  22 (the known latencies), "19, continued" (the 348 ns, reasoned), and what
  was not done.

## Shown versus reasoned

Shown on this machine (7.2.7, capabilities granted by `make`):

- `diskstats::tests::a_queue_one_account_misses_is_read_from_the_other`: the
  CI's shape (diskstats 0, sysfs 199 gives 199) and the other way round
  (diskstats 12, sysfs 5 gives 12), and a malformed sysfs file read as absent.
- `omn io.inflight -m snapshot -e raw --system` carries `inflight`,
  `diskstats_inflight` and `sysfs_inflight` on every device.
- Under eight `dd oflag=direct` writers to `nvme0n1p6`, this kernel's
  diskstats column for `nvme0n1` follows the partition's (8/8, 79/81,
  178/177): the shape the CI showed is not reproducible here.
- `scenario_io`'s queue, two-histogram and known-count scenarios green after
  the change; `cargo test --lib -- bpf::io collect::io` (87) green.

Reasoned, not shown:

- **Why the CI's disk read 0** (open; the root cause the brief asked for was
  not found): that 6.17's I/O accounting stayed on the
  partition the writes went through and never reached the disk's line. No 6.17
  source or machine here. Whether the fix turns both cells green is the next
  CI run's to show.
- **That the statistics now read the programs that ran**: this machine's
  `kernel.bpf_stats_enabled` is 0, and turning it on is machine wide. The fix
  follows from the code (the read is after the stop, and the stop closed the
  descriptors), and the coordinator's run below is what shows it.
- **Which way the 348 ns points**: the programs, most of it. Counted from what
  each request runs: dispatch about 30 to 60 ns for the two raw-tracepoint
  runs; the open (clock, an inlined lookup into a 65 536-entry map, a store)
  30 to 100 ns; the close (clock, the lookup, two `probe_read_kernel`, the
  bucket, a per-cpu row's lookup and two adds) 80 to 200 ns. Expected: `Q`, the
  run time per request counted, at 150 to 300 ns, an upper bound because the
  statistics add two clock reads per run. If so, the next two cuts are
  `tp_btf` for the two device reads, and a close that reads the request's own
  start stamp so the open program and its map go (a backend with a
  requirement: `rq->io_start_time_ns` is written only under
  `QUEUE_FLAG_STATS`). If `Q` is a small part of `N`, the criterion becomes the
  per-request cost the test prints.
- **The known-latency test**: written and compiled, never run; loading
  `null_blk` is the coordinator's.

## What could not be done, and why

- **A known number of merges**: not written. A count fixed in advance needs
  bios the plug is certain to merge, and whether `fio`'s batched io_uring
  submission plugs them (inline submission or a worker) could not be held to a
  number without a `null_blk` run of this stream's own.
- **One program for both `io.latency` histograms**: optional, not started.
- **The same zero for every other eBPF collector**: `io.fsync`, `io.pagecache`
  and `io.writeback` end their stops with `self.program = None` too, and
  nineteen places outside the family do the same. Their program types are in
  `src/bpf/io_{fsync,pagecache,writeback}.rs` and elsewhere, which this stream
  does not own; filed below, with the one fix that covers them all.

## Privileged runs wanted

All alone, on `main` with this branch merged, after `make`.

1. **The histogram's cost and the corrected statistics** (phase 6's
   criterion; the open decision):

   ```console
   cd /home/moha/repos/omniscient && make
   sudo -n /usr/bin/modprobe null_blk queue_mode=2 irqmode=0 completion_nsec=0 nr_devices=1 gb=4 bs=4096 hw_queue_depth=256 submit_queues=8
   udevadm settle --timeout=10
   sudo -n /usr/bin/chmod a+r /dev/nullb0
   cargo test --test scenario_io -- --ignored --exact \
       the_in_kernel_histogram_costs_a_two_million_iops_load_under_one_percent --nocapture
   sudo -n /usr/bin/sysctl kernel.bpf_stats_enabled=1
   cargo test --test scenario_io -- --ignored --exact \
       the_in_kernel_histogram_costs_a_two_million_iops_load_under_one_percent --nocapture
   sudo -n /usr/bin/sysctl kernel.bpf_stats_enabled=0
   sudo -n /usr/bin/rmmod null_blk
   ```

   The second run's *"the programs ran R times for T ns: P ns per run, Q ns
   per request counted"* must now show `R` near twice the counted requests;
   `Q` against the first run's `N` decides the criterion (above).

2. **The known latencies** (defect 22). This needs a `null_blk` load the
   sudoers file does not list yet (`irqmode=2 completion_nsec=3000000`):

   ```console
   sudo -n /usr/bin/modprobe null_blk queue_mode=2 irqmode=2 completion_nsec=3000000 nr_devices=1 gb=1 bs=4096
   udevadm settle --timeout=10
   sudo -n /usr/bin/chmod a+r /dev/nullb0
   cargo test --test scenario_io -- --ignored --exact \
       a_device_with_a_known_latency_puts_every_request_in_its_bucket_in_both_histograms --nocapture
   sudo -n /usr/bin/rmmod null_blk
   ```

   Sudoers line wanted for it, beside the two `null_blk` loads already there:
   `/usr/bin/modprobe null_blk queue_mode=2 irqmode=2 completion_nsec=3000000 nr_devices=1 gb=1 bs=4096`.

## Decision lines for `docs/DECISIONS.md`

- **`io.inflight` reads both of the kernel's in-flight accounts and reports
  the larger, carrying each.** `/proc/diskstats`' column and
  `/sys/block/<dev>/inflight` are counted differently (accounting per
  partition and disk, against requests the driver holds), and on the CI's 6.17
  the disk's diskstats line read 0 under a queue of 199. Each is a lower bound
  of what was outstanding, so the larger is too; keeping both visible is what
  lets a kernel where they part be seen rather than averaged.
- **A collector's stop detaches its programs and does not close them.** The
  run reads the programs' run time after the stops; a closed program has no
  account to read. (For the coordinator: the general form of this, reading the
  meters before the stops, is filed to `src/collect/mod.rs`'s owner.)
- **Phase 6's criterion** stays open until run 1 above; this stream's
  expectation is that the programs, not the dispatch, are most of the 348 ns.

## Needs from another stream

### To launcher (`src/collect/mod.rs`): read the in-target meters before the stops

`Spending::close` reads every `InsideMeter` after the final stop loop
(`collector.stop()` at the end of the run, `Collect::detach` already reads
before its drop). A collector whose `stop()` drops its programs leaves the
meter its last reading, which without a `--budget` is its first: zero. The
block collector no longer does this; about twenty others still do
(`grep -rn 'program = None' src/collect`). One `let _ =
spending.inside[index].read();` before each `collector.stop()` in the end-of-run
loop (and before `current.stop()` in `degrade`) covers every collector at once,
losing only the runs during the stop's own drain.

### To the owners of `src/bpf/io_{fsync,pagecache,writeback}.rs` and the other eBPF collectors (sched, net, mem, proc, gpu)

If the launcher's read above does not land: a `detach()` on each program type
that drops its links and keeps its programs, called from the collector's stop
in place of `self.program = None`, as `BlockProgram::detach` does.

### To the coordinator (`docs/dev/STATE.md`, sudoers)

- `docs/dev/STATE.md` "Needs privilege" item 1's commands: `udevadm settle
  --timeout=10` between the `modprobe` and the `chmod`.
- The `irqmode=2 completion_nsec=3000000` load above, in
  `/etc/sudoers.d/omniscient-privileged`, and item 11's "a known set of device
  latencies" moved to "written, run pending".
- Item 11's "driving a known number of merges" and "one program for both
  `io.latency` histograms" stay open.

## Review

The fix round after the adversarial review:

- **`read_histogram`'s doc comment** had been split from its function by
  `detach`'s; moved back above `read_histogram`. Fixed.
- **The known-latency test** now asserts the in-kernel note on the `hist`-only
  request and its absence on the streamed one. Fixed.
- **The queue scenario's ground truth** was no longer independent of the tool;
  the workload's own count of outstanding writes is now a second floor (above).
  Fixed. The kernel-reads floor stays: it still separates "the queue was never
  empty" from "the poll missed it".
- **The root cause on 6.17** was not found. Not fixable here (no 6.17 source
  or machine); `docs/scenarios/io.md` defect 20 is now titled *worked around
  in the source; the cause still open*, and the coordinator should keep it
  open in `docs/dev/STATE.md` and `docs/open-findings.md`.
- **The catalog entry**: caveat and backend comment added. Fixed.
- **An unreadable sysfs file** now names the device in the run's notes
  (`Collector::notes`), held by `queue_depth_is_reported_as_it_stands`. Fixed.
- **The silent skip** in `a_queue_one_account_misses_is_read_from_the_other`
  now prints a `skipped:` line. Fixed. (The module's older tests keep their
  silent return; not touched.)
- **`io.fsync`, `io.pagecache`, `io.writeback`**: left as filed to launcher
  (one read of the meters before the stops covers every collector), with the
  per-program `detach()` as the fallback. Their program types are in
  `src/bpf/io_*.rs`, which no stream owns this wave.

## Checks

One `make check` at load 26 stopped red on
`budget::the_extra_cpu_a_faster_poll_costs_is_the_extra_cpu_the_shell_observes`
(flaky tally 14), which passed rerun alone. Then one
`cargo test --workspace --no-fail-fast` at load 26 to 54 (ten streams):
**1896 passed, 4 targets red, 8 ignored**, none in this stream's files:

- `cases::every_case_names_its_known_cause_both_ways`:
  `sqlite3-stripped/a-plan-that-scans`, `on-cpu` 41.3 ms from
  `timer.cpu_ns` against a 38.7 ms allowance (load).
- `cross_machine::a_memory_limit_turns_a_cached_read_into_device_reads_and_each_byte_carries_its_rung`:
  the poller's ticks did not bracket the program; **passed rerun alone**.
- `runner::a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed`
  (a task's `short.dropped` 105 220 against its ring's 101 417) and
  `triage::a_verdict_read_off_a_lossy_profile_says_its_amount_is_a_lower_bound`
  (`limit` None where `cpu` was expected): both **failed again rerun alone**,
  at load 46. Both are `cpu.profile` losses under a saturated machine; this
  branch touches no file on their path (`git diff 9cd5409 --stat`: io files
  only). Filed for the owners of `src/collect/cpu/profile.rs` (profile) and
  `src/triage/**` (triage) to rerun on a quiet machine.
- cargo named `cross_io` among the red targets and not `cross_machine`; the log shows `cross_io`'s seven tests `ok` and the panic above under `cross_machine`, so the name in cargo's list is read as that one.

`cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` clean
(after one redundant closure in the new test). `scenario_io`'s queue,
two-histogram and known-count scenarios, and `cargo test --lib -- bpf::io
collect::io`, green.

### After the review round (`5e653c7`)

One `make check` at load 27 stopped red on `cases::every_case_names_its_known_cause_both_ways`
(`cpython-interpreter/one-function-dominates`: the person's way named no
state where the agent's named `on-cpu`; a load-sensitive case, no io file on
its path). Then one `cargo test --workspace --no-fail-fast` at load 21 to 28
(other streams running): **2183 passed, 9 failed, 11 ignored**, eight targets:
`cross_compute` (three states), `cross_io`
(`every_completion_is_paired_with_the_wakeup_that_released_its_reader`, "a
table for the reader"; **passed rerun alone**), `family_sched` (task moved
between two cpus), `runner` and `triage` (the two `cpu.profile` loss tests
already filed above, plus `a_layer_this_run_cannot_serve_...`),
`scenario_net` (closed-port datagrams), `scenario_proc` (defect 1's first
reading), `unwind` (dwarf). None touches `src/collect/io/diskstats.rs`,
`src/bpf/io.rs` or `catalog/io.yaml`'s `io.inflight`; `scenario_io` was green
in that run, and `cargo test --lib collect::io::diskstats` (17) and
`bpf::io` (36) green. Clippy (`-D warnings`) and `cargo fmt --check` clean, run
separately since the red `make check` stopped before them.
