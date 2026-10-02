# The sixteenth wave, stream io

Branch `feat/w16-io`, from `main` at `37f17c8`. Six items, all landed; the
details of each defect are in `docs/scenarios/io.md`, "The sixteenth wave"
(defects 14 to 18).

## The machine this ran on

The disk under `/home` was full several times during the run (0 bytes free,
`No space left on device` in builds and tests of every stream), and the load
average stood between 14 and 22. Consequences: builds ran with
`CARGO_INCREMENTAL=0`, and the last ones with
`CARGO_PROFILE_DEV_DEBUG=line-tables-only` so the full suite's test binaries
fit; the one `make check` and the one `cargo test --workspace --no-fail-fast`
were both cut short, the first by the `tests/cli.rs` red below (make stops at
the first red binary), the second by the disk filling under
`scenario_io` (34 of the test binaries ran: 1791 passed, 1 failed -- the
`cli.rs` one -- before `No space left on device` killed the run). The
wave-15 worktrees' `target/` directories (`omniscient-{docs,engine,kernel,
probes,resolver,runner}`, about 38 GB) and the main checkout's (25 GB) are
the disk; none was touched.

What was run green, after the last change, with the capabilities: `cargo test
--lib validate` (22), `cargo test --test scenario_io` (41 + 1 ignored, three
times; two reds seen in earlier runs under the disk-full load are below),
`--test family_io` (13), `--test dm_delay` (1, run for real), `cargo clippy
--all-targets -- -D warnings`, `cargo fmt --check`. Without the capabilities
(a relink): `scenario_io`, `family_io` and `dm_delay` green, the skip lines
read.

## Landed

1. **The two CI reds** (both cells).
   - `a_cold_read_of_a_known_file_is_measured_against_the_bytes_it_had_to_move`:
     the merge count held to the kernel's own merge columns over the enclosing
     window, not to zero under `none`. Not a tool defect: `none` still plug-merges
     (defect 14). `Counters` gained `merges`.
   - `a_burst_of_writes_shows_up_as_a_queue_on_the_device_that_took_it`: eight
     `O_DIRECT` writers for the whole session; the test reads
     `/sys/block/<dev>/inflight` itself every millisecond and requires the
     depth only when a 20 ms poll could not have missed the queue (two
     intervals without a break, or `(1-f)^n <= 1e-3`); the volume floor is the
     writes between the series' first and last reads, both sides in
     `CLOCK_MONOTONIC` (defect 15).
   - `pressure_is_a_share_of_its_own_window_and_full_never_exceeds_some`: the
     same writers for 600 ms (it failed once here at load 20).
   - The eBPF skip lines (`no_bpf_here`, `no_fentry_here`) put the backends'
     missing pieces first; they printed `... no viable backend on this
     machine:.` (defect 17).
   - `.github/expected-skips.txt` and `-stock.txt`: the wordings of this wave
     and the last that are the cell's (listed with their reasons in the files);
     the four that are a test's defect are not added, and are filed. Checked by
     running `.github/scripts/skip-gate.sh` over both CI logs: what remains is
     those four on `defaults` (plus the `mem.faults` refusals stream mem fixed)
     and the bpftool line on both.
   - `.github/workflows/ci.yml`: the bpftool the `linux-tools` package installs
     is linked into `/usr/local/bin`, because Ubuntu's wrapper misses it on the
     azure flavour.
2. **`src/collect/mod.rs`**: `pub(crate) mod io` (and `pub(crate) use
   block::histogram_is_enough` in `io/mod.rs`); the continuation at the
   `blind_to_running_threads` refusal; stream cpu's sixteenth-wave request
   (a late attach primes the kernel's symbols only for a stack with a kernel
   half). **`pub mod psi` is not moved**: the declaration is still in
   `src/collect/mem/mod.rs` (not this stream's), and two are clippy's
   `duplicate_mod`. `io/pressure.rs` reads `crate::collect::psi`; the three
   other lines are filed for one commit at the merge.
3. **`src/validate.rs`**: a join on any key but `dev` over a request that
   `histogram_is_enough` answers is refused, exit 2, naming the request and the
   emit that would carry the key.
   `validate::tests::a_join_over_a_histogram_built_in_the_kernel_is_refused_with_the_emit_that_serves_it`;
   by hand, `omn -f` of such a session exits 2 with the reason.
4. **`catalog/io.yaml`**: `keys: [dev]` on `io.bytes`'s and `io.iops`'s
   `fs:proc`, `keys: [dev, cgroup]` on `io.bytes`'s `fs:cgroup`, and the caveat
   that said a task-keyed request was answered silently.
   `scenario_io::a_request_keyed_on_what_the_counter_files_do_not_name_is_refused_with_the_backend_that_does`
   (both sources, `task` and `blk`, exit 3 naming `bpf:raw_tp`; `-k dev` held
   to `/proc/diskstats`'s devices).
5. **Edge-crossers**: the cause is the clock, not the buckets (both sides
   bucket `[2^(k-1), 2^k)` as `k`): two program sets read
   `bpf_ktime_get_ns` 120 ns apart at the median, -350 to +570 ns, 11.5 us
   once. The 2 % tolerance was 115 ns at 5.7 us. The test carries a witness
   set, pairs it with the stream by `blk:rq@open_ts`, and bounds the fewest
   edge crossings by the requests within twice the run's p99 skew of an edge
   plus the witnessed outliers (defect 16). Twelve runs here, zero crossings.
6. **dm-delay**: `tests/dm_delay.rs`, run here: 24 direct I/Os through 20 ms,
   481 ms at the syscall layer, 0.35 to 1.1 ms on the loop device, 480 ms
   above the device against 456 configured (24 x (20 ms - one tick)). Cleans
   up on panic (seen: a failing run left no device). Skips with the exact
   missing piece (defect 18).

## CI expectation (defaults / stock)

- `a_cold_read_of_a_known_file_...`: **green / green**. The CI's 11 and 12
  merges are within the kernel's own count by construction of the check; the
  bytes checks already passed there.
- `a_burst_of_writes_shows_up_as_a_queue_...`: **green / green**, either with
  the depth required (if the kernel's own reads see the cloud disk's queue
  held by eight direct writers, the tool's poll must too) or with `skipped the
  depth: ...` and its numbers (in `expected-skips.txt`); the points and the
  volume halves run on both. If the depth is required and the tool still
  reads 0 while the kernel's file reads more, that is a real defect in
  `io.inflight`, and the message prints both.
- `a_request_keyed_on_what_the_counter_files_...` (new): **green / green**
  (exit 3 does not depend on the capabilities: checked with a copy carrying
  none). The `fs:cgroup` half runs where `/sys/fs/cgroup/io.stat` exists.
- `a_histogram_built_in_the_kernel_...`: skips on both (no CAP_BPF), now
  with the backends' reasons first.
- `dm_delay`: skips on both, on the tracers' capabilities, before anything is
  asked of root.
- `validate::tests::a_join_over_a_histogram_...`: green / green (no machine
  dependence).
- **`tests/cli.rs::a_request_keyed_on_what_its_backend_does_not_attach_...`:
  red on both, and here**, until its owner takes the filed two-line diff: the
  test anticipated the shipped catalog's refusal as exit 2, and it is exit 3
  (the backend that attaches the key is unwritten).
- The skip gate: still red on `defaults` for the four test-defect skips filed
  (`cli.rs` net.hwts, `scenario_cpu` container note, `scenario_mem` json
  warning, `unwind` bare Permission denied), and on both for bpftool if the
  workflow's link does not find the binary.

## Open

- `pub mod psi` (above): one commit across `src/collect/mod.rs`,
  `src/collect/mem/mod.rs`, `src/collect/mem/pressure.rs`, filed.
- Stream runner's two requests to `src/collect/mod.rs` call `task.rs` code that
  exists only on `feat/w16-runner`: for the merge, as filed there.
- Flaky, seen once each under the disk-full load, each passing alone after:
  `scenario_io::substituted_counters_are_converted_exactly_and_their_gaps_are_named`
  (*"a counter that rewound has to say so rather than wrap"*, its namespace
  slow to build), and the `StorageFull` failures of the 64 MiB workloads when
  the disk was at 0 bytes.
- Not done: driving a known number of merges (`mq-deadline` and adjacent small
  reads); a hist `sum` from `total_ns` (filed to `src/emit.rs`).

## Needs privilege

Nothing beyond what was granted: `make`'s setcap, and `sudo -n losetup` /
`sudo -n dmsetup` for the dm-delay test, which ran. For another machine to run
it:

```console
# sudoers (visudo -f /etc/sudoers.d/omniscient-dm)
<user> ALL=(root) NOPASSWD: /usr/bin/losetup, /usr/bin/dmsetup
# udev (/etc/udev/rules.d/99-omniscient-dm.rules), then `sudo udevadm control --reload`
SUBSYSTEM=="block", ENV{DM_NAME}=="omn-*", OWNER="<user>", MODE="0660"
cd <checkout> && make && cargo test --test dm_delay -- --nocapture
```

And the disk: `df -h /home` reached 0 bytes repeatedly. Removing the stale
wave-15 worktrees' build directories frees about 38 GB (a human's call, since
they are other worktrees):

```console
for w in docs engine kernel probes resolver runner; do cargo clean --manifest-path /home/moha/repos/omniscient-$w/Cargo.toml; done
```

## For DECISIONS.md

- **A gauge is held to the kernel's own reading of the same gauge, not to a
  workload's promise.** The queue depth is required only when the kernel's
  `inflight` file, read by the test at 1 ms, says a 20 ms poll could not have
  missed it; otherwise the numbers are printed and the depth is not required.
  Aligned with "unknown never counts as available": a depth the sampling could
  not have seen is not evidence either way.
- **Two program sets on one tracepoint disagree by an absolute skew, measured
  per run by a witness**, rather than a relative tolerance assumed: the
  in-kernel and streamed histograms are compared with that bound. Bit equality
  would need one program feeding both.
- **A `KeyNotCarried` whose attaching backend is unwritten is the build's gap
  (exit 3)**, as `KeyElsewhere::whose` already says; the catalog's first
  shipped `keys:` make it reachable.
- **A join over a request answered by an in-kernel histogram is refused by the
  validator** (exit 2), since the collector cannot see joins: the request is
  fine alone and wrong only with the join, which is the session's error.

## Filed for other streams

In `docs/open-findings.md`, "Needs from another stream — the sixteenth wave,
stream io": the `psi` move (mem's two files); `tests/cli.rs`'s exit code for
the shipped `KeyNotCarried` (the one red this branch causes); `src/emit.rs`'s
hist `sum` of kernel rows; `src/bpf/io.rs`, one program for both histograms
(optional); four skips whose reason is not the machine's (`tests/cli.rs`,
`tests/scenario_cpu.rs`, `tests/scenario_mem.rs`, `tests/unwind.rs`); the
bpftool link; and the answers to the cpu and runner requests to
`src/collect/mod.rs`. Checked at the end of the run: the other w16 branches'
`docs/open-findings.md` carry one skip wording for this stream (mem's
*"samples page faults in user mode only"*, landed) and nothing else.

## Session index rows

None: no session file was added under `examples/` (`io-known-volume.yml`'s
comment on merges was corrected; its header is unchanged).
