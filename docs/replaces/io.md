# What omn replaces: the `io` family

The pairs are in `catalog/references/io.yaml`, the parsers (`fio_json`,
`iostat_json`, and `omn_summary_as_fio` / `omn_summary_as_iostat` for
`omn`'s side) in `tests/reference/io.rs`, the workload in
`tests/reference/workloads/io/randread.fio`: 4 KiB random direct reads, one
thread, two seconds, on a file under `target/` (a block device; `/tmp` is
tmpfs here, and a direct read of tmpfs never becomes a block request).
Every verdict below is the shared runner's (`cargo test --test reference
pairs_io`), measured on the development machine, NVMe under btrfs, kernel
7.2, while ten other families measured it too.

| Tool and use | omn equivalent | Covered | Same numbers | Missing |
|---|---|---|---|---|
| `fio`: the job's completed reads (`total_ios`) | `omn trace io.latency -m trace -e summary -- fio ...` (the count) | the reads of a launched job, per device | **yes**, equal (23798/23798, 20946/20946, 25828/25828) (`fio-reads`, `rel 0.01`) | a fio job forked from the launched process (fio's default without `thread`) is not followed: omn reported 0 requests for 28807 reads in the twenty-second wave |
| `fio`: completion latency (`clat` mean) | the same run's `io.latency` mean | issue to completion at the device, which is a span inside fio's | **within 50 %, below only**: omn 18 % below fio on a quiet machine, 31-36 % with the wave's load (57.3 us against 83.1), the syscall, filesystem and wake-up share above the block layer (`fio-clat`, `rel 0.5, side: below`) | fio's `slat`/`clat` split and its percentiles: `io.latency -e hist` gives buckets, not fio's p50/p99 |
| `fio`: bytes read (`io_bytes`) | `omn trace io.bytes --system -k dir -e rate` alongside | machine-wide only | **yes**, equal or above by other readers (102223872/102223872, 109830144/109805568) (`fio-bytes`, `rel 0.05, side: above`) | per process: `io.bytes` has no tracepoint collector on this build, so `--pid` or a launch is refused |
| `fio`: operations (`total_ios`, `iops`) | `omn trace io.iops --system -k dir -e rate` alongside | machine-wide only | **yes**, above by other readers only (20720/20709, 27744/27741) (`fio-ios`, `rel 0.05, side: above`) | per process, as `io.bytes` |
| `iostat -dx`: `r/s`, `w/s` | `io.iops --system -k dir -e rate` (`per_second`) | the same `/proc/diskstats` columns | **yes**, within 0.01 % (5879.17/5879.25, 6348.90/6349.00) (`iostat-r-per-s`, `rel 0.02`) | per device by name: omn keys `dev:259+0`, iostat `nvme0n1` |
| `iostat -dx`: `rkB/s`, `wkB/s` | `io.bytes --system -k dir -e rate` | the same columns | **yes**, within 0.01 % (`iostat-rkb-per-s`, `rel 0.02`) | |
| `iostat -dx`: `r_await`, `w_await` | `io.latency --system -e summary --filter "rwbs =~ 'R'"` (the mean) | issue to completion; `io.queue` holds the insert-to-issue part iostat folds in | **within 10 us** of iostat's two decimals of a millisecond (56.9 and 56.0 us against 0.06 ms) (`iostat-r-await`, `abs 10000` ns) | a read/write split of `io.latency` as a key (it takes a filter on `rwbs` today) |
| `iostat -dx`: `rrqm/s`, `%rrqm` | `io.merges --system -k dir` | yes, from the same column | no pair: random 4 KiB direct reads merge nothing, a merge workload is the next pair | |
| `iostat -dx`: `aqu-sz`, `%util` | `io.inflight` (queue depth), none for `%util` | depth only | no pair: `%util` (the io-ticks column over the window) has no source, and `aqu-sz` against `io.inflight` needs a workload deeper than one | `%util` |
| `iostat -dx`: `rareq-sz` | `io.size -e summary` | yes, per request | no pair: the workload's requests are all 4 KiB, which would hold whatever omn read | |
| `iostat -dx`: `d/s`, `f/s` | `io.iops -k dir` (`discard`, `flush` series) | yes | no pair: the workload neither discards nor flushes | |
| `fio`: `disk_util` | `io.iops`/`io.bytes`/`io.inflight` per device | -- | no pair: fio cannot map a btrfs file to its device and prints no `disk_util` here | |
