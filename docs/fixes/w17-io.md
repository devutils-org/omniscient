# The seventeenth wave, stream io

Branch `feat/w17-io`, from `main` at `d3a829c`. One item: the in-kernel
histogram's 23.6 % at 3 M IOPS (`docs/dev/BRIEF.md`, "io"). The defect's account is in
`docs/scenarios/io.md`, "The seventeenth wave", defect 19.

## What landed

- **`src/bpf/io.rs`: the in-kernel histogram has its own program pair**,
  `histogram_open_instructions` and `histogram_close_instructions`, instead of
  the ring pair with a different ending.
  - The open reads the task only when a process was named (before the clock),
    then the clock, then one inlined hash lookup and a store. No comm, no
    task storage, no 40-byte entry: the entry is the open time, 8 bytes.
  - The close reads the clock, the entry, and the device (two
    `probe_read_kernel`), buckets, and adds to the row. No record clearing,
    no cpu id, no scope/sector/length/flags/status reads.
  - **The entry outlives its request**: inserted the first time a request
    pointer is seen, written in place after, consumed by zeroing the time
    rather than deleted. No insert, no delete, no bucket lock on the hot path.
    The map holds 65 536 pointers (`MAX_REQUESTS_SEEN`, under 5 MiB);
    `still_in_flight` counts the entries whose time is unconsumed.
  - **The rows are a `BPF_MAP_TYPE_PERCPU_HASH`**, added to with a plain
    load/add/store per cpu (no `lock xadd` on a line all cpus share), and
    summed once at the end (`PerCpuRows`, local to `io.rs`, read after the
    detach and the grace period exactly as before).
  - The ring path is unchanged; `close_instructions` lost its `Sink`
    parameter, since the ring is now its only sink.
- **`tests/scenario_io.rs`**: the cost test prints the per-request cost
  (`jobs * (1/measured - 1/bare)`) beside the percentage, and under
  `kernel.bpf_stats_enabled=1` the programs' own run time as the tool reports
  it (`overhead.in_target_cpu_ns` / `in_target_runs`, both programs summed),
  or the tool's reason when it is not known. It asserts that the histogram
  counted fio's completed reads within 1 % before it trusts the percentage.
  The 1 % assertion stays.
- **`src/bpf/io.rs`, `src/collect/io/block.rs`**: `still_in_flight` says when
  it does not know (a failed key walk or lookup) and when its count is a lower
  bound (a walk cut short), and the note says so, instead of reporting either
  as none in flight.
- **`docs/scenarios/io.md`**: defect 19, with the arithmetic, where the cost
  was, what changed, and the argument about the criterion.

## Shown versus reasoned

Shown, on this machine, with the capabilities:

- The new programs pass the verifier and measure the same thing:
  `cargo test --test scenario_io` 41 passed, 1 ignored (the cost test);
  `--test family_io` 14 passed;
  `a_histogram_built_in_the_kernel_is_the_one_the_streamed_requests_make`
  green (1 edge crossing, inside its bound), and
  `more_rows_than_the_histogram_holds_are_counted_as_lost_and_the_rest_are_exact`
  green.
- The hot paths, counted in the instruction stream by
  `bpf::io::tests::the_histogram_programs_hot_paths_call_only_what_a_row_needs`:
  open 2 `call` instructions (3 under a process filter) where it was 6;
  close 5 where it was 11; no `MAP_DELETE_ELEM`, no atomic. Calls in the
  bytecode, not helpers run: the map lookups among them are inlined by the
  verifier, which leaves 1 (2) on the open and at most 3 on the close
  (`a_histogram_close_writes_no_record_and_counts_its_row_without_an_atomic`).
  The per-cpu sum by `a_rows_per_cpu_copies_sum_into_one_value`.

Reasoned, not measured:

- **The new overhead.** No device here answers at 2 M IOPS, and loading
  `null_blk` was refused to this stream by the session's permission layer
  (and so was a rate-limited `fio` on a file under `/home`, as load on a
  shared disk). The percentage after the change is the coordinator's run.
- **Where the 781 ns went.** From the code: the open's comm and task-storage
  read and the close's six field reads fill fields the histogram never
  reads; the hash insert and delete take a bucket lock and a free-list
  operation each; the two atomic adds hit one cache line that all 8 cpus
  write 3 M times a second (one device, a constant-time device, so one or two
  rows). Which of these was the most is not known without the run.
- **Whether 1 % is the right criterion.** 1 % on that load is 25 ns per
  request for two raw-tracepoint dispatches, two clock reads, the pairing and
  the row. Argued in defect 19; settled only by the run, see below.
- **A pre-existing hazard, not fixed**: a completion whose run the kernel
  skipped leaves its entry's time unconsumed, and if the pointer's next
  request belongs to a task the open refuses, the close counts the old time.
  Same shape in the ring pair. Bounded by `skipped`, which is reported.

## Review

The fix round answered four points from the adversarial review.

- **Defect: the second run could not print per-program run times.** Right.
  `omn` carries file capabilities, so the kernel makes it non-dumpable at exec
  and its `/proc/<pid>/fdinfo` is closed to the unprivileged test; the helper
  swallowed the error into an empty list. The test no longer reads another
  process's descriptors: it reads the run time the tool reports about its own
  programs (`overhead.in_target_*`, which it reads through `/proc/self`), and
  prints the tool's reason when that is unknown. What comes back is the two
  programs summed, not each one; that is enough for the decision, which turns
  on the programs' share of the per-request cost.
- **Gap: a pass did not show the histogram measured anything.** Right, and
  shown: `fio` with `direct=1` on a `tmpfs` file did 1 580 431 reads and the
  histogram counted 46. The test now asserts the histogram counted fio's
  completed reads within 1 % (plus the 512 a stop may leave in flight). The
  parsing was checked against a run on a file on the NVMe disk: 782 587 reads,
  782 587 counted.
- **Nit: `still_in_flight` reported unknown as none.** Fixed (above).
- **Nit: helper counts mixed calls with helpers run.** Restated as `call`
  instructions, with the count the verifier leaves after inlining the lookups,
  here and in defect 19.

## What I could not do, and why

- **Measure the cost.** See above: the run is the coordinator's.
- **`tp_btf` for the device reads.** The next candidate if the programs' own
  time is most of what is left: a BTF-typed tracepoint turns the two
  `probe_read_kernel` calls into two loads. It needs `BPF_PROG_TYPE_TRACING`
  with `BPF_TRACE_RAW_TP` and an `attach_btf_id` in `src/bpf/syscall.rs` (not
  this stream's). Filed below, conditional on the run.

## Privileged runs wanted

Alone, on `feat/w17-io` at its head, after `make` in
`/home/moha/repos/omniscient-w17-io`. Two runs: the percentage without
program statistics (they cost two clock reads per run), then the programs' own
time with them.

```console
cd /home/moha/repos/omniscient-w17-io && make
sudo -n /usr/bin/modprobe null_blk queue_mode=2 irqmode=0 completion_nsec=0 nr_devices=1 gb=4 bs=4096 hw_queue_depth=256 submit_queues=8
sudo -n /usr/bin/chmod a+r /dev/nullb0
cargo test --test scenario_io -- --ignored --exact \
    the_in_kernel_histogram_costs_a_two_million_iops_load_under_one_percent --nocapture
sudo -n /usr/bin/sysctl kernel.bpf_stats_enabled=1
cargo test --test scenario_io -- --ignored --exact \
    the_in_kernel_histogram_costs_a_two_million_iops_load_under_one_percent --nocapture
sudo -n /usr/bin/sysctl kernel.bpf_stats_enabled=0
sudo -n /usr/bin/rmmod null_blk
```

What to read: the line `bare ... IOPS, under the in-kernel histogram ...:
X %, N ns per request against a 1 % budget of B ns; the histogram counted C
requests of fio's R reads`, and in the second run the line `the programs ran
... times for ... ns: P ns per run, Q ns per request counted` (open and close
summed; Q against N is the split the decision below needs). If that line
reads "not known" instead, it carries the tool's reason. For
comparison, the same first command on `main` read 23.57 % (781 ns).

## Decision lines for docs/DECISIONS.md

- **The in-kernel block histogram has its own program pair** (wave 17): the
  open writes only the time, into an entry that outlives its request and is
  consumed in place; the close reads only the device and adds to a per-cpu
  row. Reason: the shared pair cost 781 ns per request at 3.16 M IOPS,
  reading fields no row carries, taking two bucket locks and two atomic adds
  on a line every cpu wrote.
- **Open, pending the coordinator's run: the phase 6 criterion.** If the run
  reads above 1 % with the programs' own run time a small part of the
  per-request cost, the criterion becomes a per-request cost in nanoseconds
  on this load (the figure the test prints), since 1 % of a device that does
  no work is 25 ns for two tracepoint dispatches and two clock reads. If the
  programs' own time is most of it, `tp_btf` is next (below).

## Needs from another stream

- **core / owner of `src/bpf/syscall.rs`** (conditional on the run): loading a
  `BPF_PROG_TYPE_TRACING` program with `expected_attach_type =
  BPF_TRACE_RAW_TP` and an `attach_btf_id` of `btf_trace_<tracepoint>`, and
  attaching it with `raw_tracepoint_open(NULL, fd)` or `BPF_LINK_CREATE`, so
  `src/bpf/io.rs` can read `request.part->bd_dev` with two loads instead of
  two `probe_read_kernel` calls.
- **Coordinator**: the privileged runs above, and the result into
  `docs/scenarios/io.md` defect 19 and `docs/dev/STATE.md`.

## The suite

After the fix round: one `make check`, red on
`scenario_sched::one_known_number_of_round_trips_is_found_by_four_sources`
(2513 waits for 2000 round trips; in `docs/open-findings.md`'s tally). Rerun
alone once: passed. Then one `cargo test --workspace --no-fail-fast`: 2191
passed, 0 failed, 10 ignored. `cargo clippy --workspace --all-targets -- -D
warnings` and `cargo fmt --all --check` clean. Touched while iterating:
`cargo test --lib bpf::io` 36 passed, `--lib collect::io::block` 14 passed,
`--test scenario_io` 41 passed, 1 ignored (the cost test, which needs
`null_blk`).
