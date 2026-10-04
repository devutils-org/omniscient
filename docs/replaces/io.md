# What omn replaces: the `io` family

The pairs are in `catalog/references/io.yaml`, the parsers (`fio_json`,
`iostat_json`) in `tests/reference/io.rs`, the workload in
`tests/reference/workloads/io/randread.fio`: 4 KiB random direct reads, one
thread, two seconds, on a file under `target/` (a block device; `/tmp` is
tmpfs here, and a direct read of tmpfs never becomes a block request).
Numbers measured on the development machine, NVMe under btrfs, kernel 6.17.

| Tool and use | omn equivalent | Covered | Same numbers | Missing |
|---|---|---|---|---|
| `fio`: the job's completed reads (`total_ios`) | `omn trace io.latency -m trace -e summary -- fio ...` (the count) | the reads of a launched job, per device | **yes**, equal on a warm file over three runs (29335/29335, 28837/28837, 29118/29118); +1.9 % on the run that lays the file out, btrfs checksum-tree reads in fio's context (pair 1, `rel 0.03`) | a fio job forked from the launched process (fio's default without `thread`) is not followed: omn reports 0 requests for 28807 reads |
| `fio`: completion latency (`clat` mean) | the same run's `io.latency` mean | issue to completion at the device, which is a different span | **within 25 %**, omn below fio by 18 % (55.9 us against 68.0 us): the syscall and filesystem share above the block layer (pair 2, `rel 0.25`) | fio's `slat`/`clat` split and its percentiles: `io.latency -e hist` gives buckets, not fio's p50/p99 |
| `fio`: bytes read (`io_bytes`) | `omn trace io.bytes --system -k dir -e rate` alongside | machine-wide only | **yes** on a quiet disk (121532416/121532416; 122044416 keyed on dev) (pair 3, `rel 0.05`) | per process: `io.bytes` has no tracepoint collector on this build, so `--pid` or a launch is refused |
| `fio`: operations (`total_ios`, `iops`) | `omn trace io.iops --system -k dir -e rate` alongside | machine-wide only | **yes** on a quiet disk (29671/29671) (pair 4, `rel 0.05`) | per process, as `io.bytes` |
| `iostat -dx`: `r/s`, `w/s` | `io.iops --system -k dir -e rate` (`per_second`) | the same `/proc/diskstats` columns | not run: `iostat` is not installed (pair 5 skips) | per device by name: omn keys `dev:259+0`, iostat `nvme0n1` |
| `iostat -dx`: `rkB/s`, `wkB/s` | `io.bytes --system -k dir -e rate` | the same columns | not run: `iostat` is not installed (pair 6 skips) | |
| `iostat -dx`: `r_await`, `w_await` | `io.latency --system -e summary` (mean) | issue to completion; `io.queue` holds the insert-to-issue part iostat folds in | not run (pair 7 skips) | a read/write split of `io.latency`'s mean in one run |
| `iostat -dx`: `rrqm/s`, `%rrqm` | `io.merges --system -k dir` | yes, from the same column | no pair: random 4 KiB direct reads merge nothing, a merge workload is the next pair | |
| `iostat -dx`: `aqu-sz`, `%util` | `io.inflight` (queue depth), none for `%util` | depth only | no pair | `%util` (the io-ticks column over the window) has no source |
| `iostat -dx`: `rareq-sz` | `io.size -e summary` | yes, per request | no pair | |
| `iostat -dx`: `d/s`, `f/s` | `io.iops -k dir` (`discard`, `flush` series) | yes | no pair: the workload neither discards nor flushes | |
| `fio`: `disk_util` | `io.iops`/`io.bytes`/`io.inflight` per device | -- | no pair: fio cannot map a btrfs file to its device and prints no `disk_util` here | |
