# The fifteenth wave — stream kernel

Branch `feat/w15-kernel`. The kernel programs' files: `src/bpf/**` (not
`syscall.rs`, `usdt.rs`), `src/collect/{sched,proc,io,net}/**`,
`src/collect/mem/**` (not `page_faults.rs`, `heap/**`), `src/collect/psi.rs`,
their catalog entries, scenario and family tests. The record of the family is
`docs/families/kernel.md`, "The fifteenth wave", which points here.

## 1. `net.hwts`: listed usable, refused at the start

**The defect** (the matrix's `here-caps` cell): `net.hwts` is listed usable
through `netlink:ethtool`, and the sweep's first mode, `trace`, resolved onto
that backend and was then refused by its factory -- exit 3, *"`trace` on
net.hwts means a timestamp per packet, which is the `so_timestamping` backend
and is not built"*. The resolver could not know: the chain declared no modes
per backend, so both backends claimed all three.

**The fix is the catalog's** (`catalog/net.yaml`): `so_timestamping` declares
`modes: [trace]`, `netlink:ethtool` declares `modes: [poll, snapshot]`. A
`trace` of an interface or of the machine is now passed over at resolution
with the reason (*"`netlink:ethtool` serves `poll`, `snapshot` mode, not
`trace`"*), so the sweep moves to `poll`, which runs. A `trace` of a launched
program still resolves onto `so_timestamping` and is refused as not built,
exit 3, which is right: that backend has no collector, and the judge skips it.
The factory's own refusal stays as a second line of defence.

**Test**: `scenario_net::every_usable_net_source_starts_in_every_mode_that_resolves`
asks every `net` source `omn list` calls ready, in every mode and on the
machine (or `lo`, or a launched `sleep`), what the sweep asks: resolve, and if
it resolved onto a backend this build collects, run. Every one starts on
this machine; before the fix `net.hwts --system -m trace` was the one that did
not. The one refusal allowed is the target's: `net.request` follows USDT probes
`sleep` does not carry, and says so. `the_trace_leg_of_the_sweep_...` now
asserts the resolution-time refusal for an interface (exit 1 today, 2 once P
lands the fourteenth wave's "a pair no backend declares" filing) and exit 3
for a launch.

## 2. Filed against these files in the fourteenth wave

- **`src/bpf/btf.rs:670`, the bpftool skip.** Worse than filed: the
  cross-check skipped **on every machine with bpftool**, not only the runner.
  `bpftool btf dump` prints the per-cpu `DATASEC`'s member line
  (`type_id=127879 offset=... (VAR 'runqueues')`) before the variable's own
  line, and the test took the first line containing `VAR 'runqueues'`, which
  does not parse as `[id]`, and printed "could not dump". It now finds the
  `[<id>] VAR 'runqueues' ` line, **asserts** when bpftool dumped and no such
  line exists, and skips only when bpftool exited non-zero, with its status and
  first stderr line (Ubuntu's wrapper names the `linux-tools` package it
  wants). Here: it ran, 127879 = 127879.
- **`net.softirq`'s kernel count through `facts()`**: already landed in the
  fourteenth wave (`0ad14b3`), held by `scenario_net`'s check that the count is
  in `facts` and not `notes`. Nothing to do.
- **`family_sched::a_program_that_blocks_ten_times_shows_ten_context_switches`**:
  skips on *"has no viable backend on this machine"* or *"which happen only in
  kernel mode"*, printing the `error:` line. The two `scenario_sched` tests
  named with it already did.
- **`scenario_mem`'s skip lines**: `unavailable()` returns the stderr from the
  first `error:` on, so a launch warning (*"`json` is one document..."*) is no
  longer the skip reason the CI gate reads. The counter sweeps and the
  no-PMU skips named in the same filing were already in place.
- **Flaky tally #3, `an_overflowing_wakeup_ring_says_what_it_could_not_hold`**:
  closed by item 3 below -- one run, which has to overflow, checked once.
- **Not done, and why**: the `psi` move (`src/collect/mod.rs` is R's, and
  clippy's `duplicate_mod` makes it one commit across both owners, the
  coordinator's at the merge -- the four lines are in the fourteenth wave's
  filing). `scenario_net::every_datagram_sent_to_a_closed_port_...` and
  `a_queue_that_delays_every_packet_...` (flaky, load 15 to 20): not
  reproduced here in this wave; not touched.
- **Found on the way: notes carrying their source's indentation.** A string
  continued across lines without its trailing `\` keeps the next line's
  indentation, and `sched.wakeup` printed *"the waker is the                  interrupt"*.
  Fixed in every file of this stream (`src/collect/sched/wakeup.rs` 3,
  `src/collect/io/diskstats.rs` 2, `src/collect/io/pressure.rs` 1,
  `src/bpf/insn.rs` 4). Held by the moderate-load test below, which asserts
  no `degraded:` line carries a run of spaces. Five more are in other
  streams' files, filed.

## 3. Phase 3: the loss rate is shown, and zero under moderate load

Both tests use a C fixture (`wakers`, built by `build_c` beside the others)
that prints what the kernel said it did.

- **`scenario_sched::a_moderate_wakeup_rate_loses_nothing_and_says_so`**. The
  moderate load is defined by its rate: 2 000 pipe round trips at 2 000 a
  second, paced by an absolute-deadline `clock_nanosleep`, which is about
  6 000 wakeups a second (two of the program's threads, one timer) against the
  **468 100 a second** the wakeup ring drains (256 KiB / 56-byte records every
  10 ms) -- under 1.3 %, and the test asserts the ratio stays under 2 % if
  anyone changes the constants. Asserted: the program completed its 2 000
  round trips, the run's loss line is present and says 0 dropped, and the run
  kept at least 4 000 wakeups (two per round trip, the program's own count).
- **`scenario_sched::an_overflowing_wakeup_ring_says_what_it_could_not_hold`**,
  rewritten so it fills on every machine. One `FUTEX_WAKE` of 16 384 parked
  threads is 3.5 rings' worth of records written inside one system call --
  more than even two drains landing inside the call could make room for --
  and three rounds of it. Ground truth: the sum of `FUTEX_WAKE`'s return
  values. Asserted: the ring overflowed (`dropped > 0`, never a skip), `seen +
  dropped >= woken`, the loss line in the terminal with *"ring was full"*, all
  in **one** run (the old test checked the terminal on a second run, which
  sometimes dropped nothing -- tally #3). Three runs measured by hand: 23 158,
  24 854 and 26 378 dropped against 49 152 woken, about 0.5 s and 1.1 s of cpu
  each. The Python ping-pong
  session (`examples/sched-wakeup-loss.yml`) is unchanged and still checked
  for its workload constants.

## 4. Phase 6: `io.latency --emit hist` is built in the kernel

`io.latency` streamed every completion through the ring whatever was asked.
Now, when a log2 histogram is **all** the request asks for -- one `hist`
emit, `trace`, no filter, no key but `dev`
(`collect::io::block::histogram_is_enough`) -- the closing program counts each
interval into a `(device, log2 bucket) -> (count, total_ns)` row of a BPF hash
and publishes nothing (`bpf::io::BlockProgram::load_histogram`,
`Sink::Histogram`, `count_into_histogram`). The bucket is
`insn::push_log2_bucket`'s, the emit layer's numbering; the row is shared by
every cpu so both words are added atomically; two cpus creating a row race on
`BPF_NOEXIST` and the loser adds to the winner's; a full map is a counted loss
(`DROP_HIST`). The rows are read once, at `stop`, after the closing program is
detached and a grace period waited for; each becomes one event weighed by its
count (`emit::WEIGHT_ATTR`) at its bucket's low bound, with `total_ns` exact.
The run says how it was obtained in `facts()` (*"the histogram was built in the
kernel: N requests counted into M rows..."*), and the catalog's caveat says
what a row does not carry. Anything else -- `summary` beside it, `raw`,
`topk`, a linear histogram, a filter, `-k blk` -- streams as before.

- **`scenario_io::a_histogram_built_in_the_kernel_is_the_one_the_streamed_requests_make`**:
  one launch, two requests -- `hist` alone (in the kernel) and `raw, hist`
  (streamed). The program reads 256 pages and 64 blocks of 64 KiB through
  `O_DIRECT` and counts what completed. Asserted: the streamed requests are
  of those two sizes (or a readahead), no more than the reads, at most one
  short of each (the btrfs first-read readahead); the kernel's rows hold
  **exactly** as many requests as the stream carried, on the same devices; and
  per bucket they differ only by requests within 2 % of a bucket edge (the two
  program sets read the clock tens of nanoseconds apart). Five runs: totals
  equal every time, buckets equal in most, 4 requests crossing the 8 µs edge
  in one.
- Unit: `bpf::io::a_histogram_close_writes_no_record_and_counts_its_row_atomically`,
  `a_histogram_row_decodes_where_the_program_writes_it`;
  `collect::io::block::only_a_request_for_a_log2_histogram_alone_is_answered_in_the_kernel`,
  `a_histogram_row_is_one_weighted_event_in_its_own_bucket`.
- **The 2 M IOPS overhead**: `scenario_io::the_in_kernel_histogram_costs_a_two_million_iops_load_under_one_percent`,
  `#[ignore]`d, needs `null_blk` (below). It runs `fio` bare and under the
  in-kernel histogram and asserts the IOPS ratio is within 1 %.

## Review, and what changed after it

1. **`scenario_sched::a_run_that_could_not_time_a_wait_says_which_wait_and_why`
   red once in a whole-file run** -- real. The full run after the review caught
   the reason: *"a round of reads outran the 100.0ms tick: worst 291.7ms,
   263.5ms on average"*, the collection loop's own lateness
   (`src/collect/mod.rs:557`), which is not a lost wait: the rows stay in the
   kernel's map and arrive with the next drain. The test now sets that reason
   apart (it neither needs a lost count nor fails the list), and the list of
   known reasons gained the three sentences `src/collect/sched/lock.rs` can
   write and it lacked: a row gone between listing and reading, a walk stopped
   at its bound, a failed histogram read. Passed alone after the change.
2. **`scenario_mem::a_gibibyte_of_reads_costs_one_l1_miss_per_cache_line`** --
   not this branch's: already in the flaky tally (huge-page TLB misses), and
   the branch only rewords a skip in that file. Nothing changed.
3. **The net sweep passed a ready source for which no mode resolved** -- real.
   `every_usable_net_source_starts_in_every_mode_that_resolves` now records
   every (target, mode) refusal and every resolution onto a backend this build
   does not collect, and fails a source `omn list` calls ready for which none
   resolves onto a collected backend, printing the refusals. The early return
   is now only for a machine where no net source is ready at all. Here all
   fourteen ready net sources resolve somewhere, and the test passes (16 s).
4. **Exit 1 for a `net.hwts` trace of an interface** -- not a defect of this
   branch, as the reviewer says: 2 is filed to P. The test accepts `1 | 2` so
   it does not go red the day P lands; 1 is not final.
5. **The in-kernel histogram test compared the code with itself** -- real. Each
   path is now held to the program's own account *before* the two are
   compared: each counts no more requests than the program issued (320) and,
   with what it dropped added back, no fewer than 318 (the two first reads a
   filesystem may turn into readaheads); the stream holds at most two requests
   of neither size. A request both paths count and the program never made
   now fails the total. A run with drops no longer skips: it gets the
   per-path checks with its losses counted, and only the bucket-by-bucket
   comparison is left out, loudly. Three runs, all through the full
   comparison.
6. **Misplaced doc comment in `src/bpf/io.rs`** -- real, fixed: the comment is
   back on `only_a_paired_stage_computes_an_interval` and the histogram test
   has its own. The `kernel_btf() else { return }` is not silent: the module's
   `kernel_btf()` prints `skipped: <reason>` before returning `None`
   (`src/bpf/io.rs:1199`), the pattern all ten tests there use.
7. **No green full run after the last change; a relink drops the
   capabilities** -- `make` was run before every test run here and `getcap`
   checked. See Checks for the full runs.
8. **Ownership** -- nothing to do. The two test files touched after the review
   are this stream's.

## Checks

After the review: `make check` once at load 8, red on
`scenario_net::every_datagram_sent_to_a_closed_port_is_a_drop_with_one_reason`
(in the flaky tally; passed alone once); then one
`cargo test --workspace --no-fail-fast`, red on two of this stream's
`scenario_sched` tests: the lock-loss test, whose reason it printed (review
item 1, fixed), and `every_thread_a_program_starts_has_its_switches_counted`
(113 switches seen against 108 counted plus the edge allowance; unchanged on
this branch, passed alone once). After the fix, `make check` once more at
load 37: red on `scenario_mem::a_walk_that_fits_in_l1_is_never_timed_...`
(*"13 of 52 of the small walk's loads"* timed; a cache measurement under load,
`docs/open-findings.md:7549`; passed alone once), and then
`cargo test --workspace --no-fail-fast` **green, every binary**, clippy
`-D warnings` and `fmt --check` clean, capabilities on the binary throughout.

Before the review:
`make check` once, red on two `tests/runner.rs` tests (R's) at load 37:
`a_counted_window_over_a_polled_counter_ends_with_its_closing_read` passed
alone; `a_source_attached_for_one_phase_...` (1.0186e9 and then 1.0058e9
instructions against 1e9, a 0.5 % band) failed alone once and passed at load
9. Then one `cargo test --workspace --no-fail-fast`: ten failures, one of them
this stream's -- `scenario_net::forcing_a_net_backend_nothing_implements_names_it_and_exits_three`
forced `so_timestamping` in `snapshot`, which it no longer declares; it asks
`trace` now and passes. The other nine (`budget`, `cross_compute`, `cross_io`,
`encoders`, `family_sched::a_task_moved_between_two_cpus_...`,
`scenario_mem::both_huge_page_backends_...`, `triage`, two in `unwind`) each
passed alone, once. clippy `-D warnings` and `fmt --check` clean. While iterating: `cargo test --lib bpf::btf`,
`bpf::io`, `collect::io::block`, `--test scenario_net`, `--test scenario_sched`
(the two wakeup tests), `--test scenario_io` (the histogram test).

## Landed (each with its test)

- `net.hwts` modes per backend: listing and run agree --
  `scenario_net::every_usable_net_source_starts_in_every_mode_that_resolves`,
  `scenario_net::the_trace_leg_of_the_sweep_answers_or_names_the_backend_it_would_need`.
- The bpftool cross-check runs where bpftool is, and skips with bpftool's
  answer -- `bpf::btf::tests::a_variable_is_found_by_the_id_the_kernel_numbers_it_with`.
- The sched and mem skip lines -- `family_sched::a_program_that_blocks_ten_times_...`,
  `scenario_mem` (`unavailable`).
- Zero loss under a stated moderate load, with the loss line --
  `scenario_sched::a_moderate_wakeup_rate_loses_nothing_and_says_so`.
- The overflow test fills everywhere, one run --
  `scenario_sched::an_overflowing_wakeup_ring_says_what_it_could_not_hold`.
- `io.latency --emit hist` in the kernel --
  `scenario_io::a_histogram_built_in_the_kernel_is_the_one_the_streamed_requests_make`
  and the four unit tests above.
- Ten notes that carried their indentation, in this stream's files.

## Open (with measured reason)

- **The 2 M IOPS overhead is written, not measured**: `null_blk` is a module
  load. The disk here does not reach the rate: `dd` of 64 KiB `O_DIRECT`
  reads ran at about 12 000 a second.
- **A session join over a `hist`-only `io.latency`** gets rows with no `blk`
  key: a join names sources, not requests, and reaches no collector, so this
  side cannot refuse it. The run's `note:` says what the rows carry; the
  validator is asked to refuse it (filed).
- **The streamed and kernel histograms can differ by edge-crossers** (4 of 318
  in one of five runs): two program sets, two clock reads. A test that wants
  bit equality needs one program feeding both, which is not a shape this
  layer has.
- The `psi` move: the coordinator's, at the merge (four lines, in the
  fourteenth wave's filing).

## Needs privilege

```console
# Phase 6, the 2 M IOPS overhead of io.latency --emit hist (needs fio)
sudo modprobe null_blk queue_mode=2 irqmode=0 completion_nsec=0 nr_devices=1 \
    gb=4 bs=4096 hw_queue_depth=256 submit_queues=8
sudo chmod a+r /dev/nullb0
cd /home/moha/repos/omniscient-kernel && make && \
    cargo test --test scenario_io -- --ignored --exact \
    the_in_kernel_histogram_costs_a_two_million_iops_load_under_one_percent --nocapture
sudo rmmod null_blk
```

The launched `fio` is the target (`--thread`, so its eight jobs are one
process's threads, which is what a process target keeps).

## For `DECISIONS.md`

- **A backend that serves fewer modes than its chain declares them, per
  backend** (`modes:` on the backend), rather than a factory refusing at the
  start. A refusal after the probes said yes is finding 15's shape; the
  catalog is the one place that can say it before resolution.
- **`io.latency` builds its histogram in the kernel only when a log2 histogram
  is the whole request** (one `hist` emit, `trace`, no filter, keys ⊆ `dev`).
  Anything that needs the requests one at a time streams. Aligned with "an
  invalid combination is refused, never degraded silently": the choice changes
  nothing the request asked for, and the run says which it was in `facts()`.
  Not taken: making it the default for `hist` plus `summary` (the summary's
  min and max would become bucket bounds, a silent change of meaning).
- **The overflow test fills by construction, not by the machine being slow**:
  a burst larger than the ring inside one system call, with the kernel's own
  return value as the ground truth.

## Filed for other streams

Appended to `docs/open-findings.md`, "Needs from another stream — the
fifteenth wave, stream kernel":

- P (`src/validate.rs`): refuse a join over a `hist`-only `io.latency`;
  R (`src/collect/mod.rs`): `mod io;` to `pub(crate) mod io;` so the validator
  can call `histogram_is_enough`.
- Owners of `src/perf.rs:3457`, `src/collect/cpu/counters.rs:1354,1401`,
  `src/collect/mod.rs:2637`, `tests/scenario_runtime.rs:240`: the same
  lost-`\` continuation.
- The coordinator: `docs/scenarios/net.md` (defect 7's record), `docs/scenarios/sched.md`
  (the loss workload), `ROADMAP.md` phase 3 and 6 audit notes, the flaky tally
  (#3 closed), `.github/expected-skips.txt` (the bpftool skip wording).

## Session index rows

None: no session file was added under `examples/`. The histogram test writes
its two-request session to the test's scratch directory.
