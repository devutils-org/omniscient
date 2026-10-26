# The twentieth wave, stream io

Branch `feat/w20-io`, from `main` at `444900b`. `docs/dev/BRIEF.md`, "io -- the
storage sources the views found short" (`docs/dev/STATE.md` items 8, 11 and 2): a
filesystem's superblock device mapped to the block devices under it;
`io.bytes` split by direction; `raw` on `io.bytes` and `io.syscall`; a known
number of merges; `io.inflight`'s cause on 6.17; optionally one program for
both `io.latency` histograms.

## What landed

- **`blk_devs` on `io.fs`, `io.fsync` and `io.pagecache`** (`0597d36`). A
  new `src/collect/io/underneath.rs` follows a mount source through `/sys`
  -- a partition to its disk, dm and md to their members, a btrfs to every
  device under `/sys/fs/btrfs/<fsid>/devices` -- down to the devices
  `io.bytes`'s `fs:proc` reports, and spells each as `io.bytes` keys its
  series (`dev:259+0`), comma-separated. `vfs::Mounts` keeps each mount's
  source and caches the answer per superblock; where the source is not a
  name `/sys` knows (the CI's `/dev/root`), it tries the superblock's own
  `major:minor`. Absent, not empty, on a filesystem on no block device.
  Caveats added to the three catalog entries. Defect 23 in
  `docs/scenarios/io.md`.
- **`io.inflight`'s 6.17 zero, explained** (`3d621f4`). Read from the
  kernel's tree at `v6.17` and `v7.2`: before 7.2, `blk_account_io_start`
  increments the per-cpu `in_flight` with `part_stat_local_inc(req->part,
  ...)`, which touches the partition only (unlike `part_stat_inc`), and
  diskstats' ninth column for the disk reads the disk's own counters, so a
  queue made through a partition reads 0 there. Upstream `5bdb8ec58b54`
  ("block: propagate in_flight to whole disk on partition I/O", in v7.2,
  not in v7.1) fixes it. The collector's doc comment, the catalog caveat and
  defect 20 now say so; the workaround (the larger of diskstats and sysfs)
  stays and is now explained.
- **Merges, measured** (in `docs/scenarios/io.md`, defect 24): sixteen
  adjacent direct reads in one `io_uring` submission became two requests and
  **one** merge, three times out of three; libaio on btrfs went through the
  page cache. A merge count is not predictable from adjacent bios here, so
  no test holds one.
- **`raw` on `io.bytes` and `io.syscall`**: both already serve it, checked
  rather than changed: `io.bytes -m poll -e raw --system` returns events with
  `read_bytes`, `write_bytes`, `sector_bytes`, `discarded_bytes`;
  `io.syscall -m trace -e raw -- dd ...` returns rows with `fd`, `op`,
  `bytes`, `syscall`, keyed by task. What is missing is the views asking
  for it, in their needs: that is the view stream's (`docs/view/README.md`,
  `tests/view_model.rs`).

## Shown versus reasoned

- **Shown**: `io.fs -m trace -e raw -- dd ... conv=fsync` on the scratch
  btrfs: every event on `0:28` carries `blk_devs: dev:259+0`, and
  `io.bytes -e summary --system -d 1s` reports `dev:259+0` (with `dev:253+0`
  and `dev:7+0`). The scenario test
  `a_file_event_names_the_block_devices_its_filesystem_is_on` ran (not
  skipped) with the capabilities `make` granted. The merge counts and the
  libaio fallback, measured with `fio` and `/proc/diskstats` on `nvme0n1`
  while other streams were building.
- **Reasoned**: the 6.17 cause is read from source, not run on a 6.17
  kernel. The CI's next log -- `diskstats_inflight` at 0 where
  `sysfs_inflight` is not, on the 6.17 runner -- is what would show it. The
  `/dev/root` fallback through the superblock's number is reasoned (no such
  machine here). The multi-device and stacked cases are held on fabricated
  `/sys` trees only.

## Not done, and why

- **`io.bytes` split by direction in timeline/summary/rate.** The split is
  on every event and `raw` carries it; a series per direction needs a key
  to group on, and `KeyKind` in `src/request.rs` has none. That file is not
  this stream's. See the request below; `net.bytes` wants the same.
- **A known number of merges**: see defect 24. A test holding a count would
  need a kernel whose merge accounting follows the bios, or `null_blk`
  (a module, not loaded here).
- **Adding the partitions' in-flight columns to the disk's on kernels before
  7.2** (to recover requests in the scheduler that the sysfs walk misses):
  not done; the partitions are lines the collector leaves out on purpose,
  and the CI's `none` scheduler holds nothing there.
- **One program for both `io.latency` histograms**: optional, not started.

## Tests run

- `cargo test --lib collect::io`: 60 passed before the second commit, then
  `collect::io::diskstats` 17 and `collect::io::underneath` 7 passed.
- `cargo test --lib catalog`: 59 passed (twice, after each catalog edit).
- `cargo test --test family_io`: 14 passed.
- `cargo test --test scenario_io a_file_event_names`: 1 passed, not skipped.
  The rest of `scenario_io` was not run (its own tests, untouched).
- `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo fmt --check`: clean.

## Privileged runs wanted

None new. The scenario test above takes the eBPF path with the
capabilities `make` grants; it needs no sudo. For the 6.17 cause, the CI's
next run of
`cargo test --test scenario_io a_burst_of_writes_shows_up_as_a_queue_on_the_device_that_took_it -- --nocapture`
on the 6.17 runner, reading `diskstats_inflight` against `sysfs_inflight`
in its output.

## Decision lines

- **A filesystem's devices are an attribute, not a key.** `blk_devs` is a
  set (a btrfs spans several), so it cannot be a single `dev` key without
  choosing one; a reader joins on it by splitting. Leaving it an attribute
  costs a view one split per event; making it a key would need a set-valued
  key, which nothing has.
- **A direction key for byte counters** (proposal): `KeyKind::Dir`
  (`dir:read`, `dir:write`), produced by `io.bytes`, `io.iops`, `io.merges`
  and `net.bytes` as one event per direction per poll, would give every
  aggregating emit the split without a new emit. Leaving it open costs the
  views `raw` for a split they draw live and lose in `results.json`.

## Needs from another stream

- **Coordinator (`src/request.rs`, unowned)**: `KeyKind::Dir` as above. Once
  it exists, the io stream emits two events per device per poll in
  `src/collect/io/diskstats.rs` and declares `dir` in `catalog/io.yaml`.
- **view**: `io.syscall` as `raw,summary,hist` in flows' and ledger's needs,
  and `io.bytes` as `raw` in system's (both sources serve it today); flows
  can now draw the file -> device edge by matching `io.fs`'s `blk_devs`
  against `io.bytes`' series keys, and `docs/view/flows.md`'s "It does not
  draw ... file -> device" paragraph can say so.
- **Coordinator (`docs/dev/STATE.md`)**: item 11's "`io.inflight`'s cause on 6.17"
  is settled (defect 20); item 8's "a filesystem's superblock device mapped
  to the block devices under it" is done for `io.fs`, `io.fsync` and
  `io.pagecache`.
