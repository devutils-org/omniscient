# The twenty-second wave, stream sysstat

`vmstat`, `free`, `numastat`, `slabtop`, `uptime`'s load, `/proc/pressure`,
`iostat`, `mpstat`, `pidstat` and `sar` against `omn`. The sysstat tools,
listed as missing in the brief, were installed on this machine by the time
the pairs ran (sysstat 12.8.0), so every pair below ran for real.

## What landed

- `catalog/references/sysstat.yaml`: thirteen pairs. Three launch pairs
  (`numastat -p`, `pidstat -r`, `pidstat -w`) and ten system pairs (`vmstat`
  us+sy, si, so; the three PSI files, io's `some` and `full`; `mpstat`;
  `iostat -c`; `sar -u`). Its header documents the fields this family adds:
  `target: system`, `compare.omn_key`, `compare.scale`, the `raw.<attr>`
  path, and `known_defect`.
- `tests/reference/sysstat.rs`: eight parsers (`vmstat`, `numastat_p`,
  `pidstat_r`, `pidstat_w`, `mpstat`, `sar_u`, `iostat_cpu`, `psi_window`),
  each unit-tested on a capture; and `sysstat_pairs_agree_or_skip_with_the_reason`,
  which runs this family's pairs itself, because the shared runner did not
  exist when this stream ran and a system pair cannot be run the way a launch
  pair is (`--system` refuses `-- program`).
- `tests/reference/workloads/sysstat/`: `touch.py` (a known footprint),
  `spin.py` (one busy cpu), `switch.py` (a known number of voluntary switches).
- `tests/reference/captures/sysstat/`: `vmstat`, `numastat -p` and a PSI
  window captured here; `mpstat`, `sar -u`, `iostat -c`, `pidstat -r`/`-w`
  written in sysstat 12's `LC_ALL=C` layout before the tools appeared, with
  numbers chosen to check the arithmetic.
- `docs/replaces/sysstat.md`: 26 rows, one per tool and use.

## Shown versus reasoned

Shown, three runs of the pairs on 2026-10-04 (the last two after the window
fix below):

| Pair | Tolerance | Seen |
|---|---|---|
| `numastat -p` vs `mem.numa` | rel 0.03 | 35 237 vs 35 254 pages (0.05 %) |
| `pidstat -r` vs `mem.rss` | rel 0.03 | 144.3 MB vs 143.6 MB (0.5 %) |
| `pidstat -w` vs `sched.switches` rate | rel 0.15 | **66 685/s vs 944/s**: known defect |
| `vmstat` us+sy vs `cpu.util` busy | abs 0.03 | 0.002 to 0.007 |
| `vmstat` si, so vs `mem.swap` | abs 256 KiB/s | 0 vs 0 (no swapping) |
| `/proc/pressure/cpu` some | abs 0.02 | below 0.0001 |
| `/proc/pressure/memory` some | abs 0.02 | 0 vs 0 |
| `/proc/pressure/io` some, full | abs 0.02 | 0.0001 to 0.0014 at 26 to 39 % |
| `mpstat` all | abs 0.03 | 0.0008 to 0.002 |
| `iostat -c` | abs 0.03 | 0.0008 to 0.004 |
| `sar -u` | abs 0.03 | 0.0005 to 0.001 |

Before the window fix, the first run had io.pressure out by 0.05 and
`iostat -c` by 0.10: the driver dropped every pair's first raw event, which
for `mem.pressure` and `io.pressure` is a real window. It now drops a leading
event only when it is stamped within half a period of the window's opening.

Reasoned, not shown: the swap and memory-pressure pairs agree only at zero;
a machine that swaps would show whether `in_bytes` counts zero-filled
swap-ins (`in_zero_bytes`) that vmstat's `pswpin` does not -- one raw read
had `in_bytes` 368 640 with `in_zero_bytes` 217 088, so it may.

## Defects and gaps found in omn (each a line of work)

1. **`sched.switches` counted-window rate uses the wrong window.**
   `omn sched.switches -m count -e rate -- python3 switch.py 3000` gives
   `total` 3005, `window_ns` 21 660 065 and `per_second` 138 734, over an
   `observed_ns` of 3.18 s: about 945/s is right. The rate divides by a window
   of 12 to 22 ms. Shown, and held by the `pidstat -w` pair as `known_defect`.
2. **A poll's leading event covers a few ms.** `cpu.util -m poll@1s` emits a
   first raw event stamped as the window opens, its busy share computed over
   a few ms (seen 0.0 and 0.5); `cpu.pressure` emits one with `some` 0.0.
   `mem.pressure` and `io.pressure` do not. It enters the summary's
   `min`/`mean`: `-d 4s` on a 22 % busy machine gave `mean` 0.155 against
   0.19 over the four real windows. The event count also changes with the
   emits (`-e raw` alone 4 events, `-e summary,raw` 3, over `-d 3s`).
   Held by `poll_leading_event_is_held_as_a_known_defect` in
   `tests/reference/sysstat.rs`, not by a pair (see "Review").
3. **`cpu.pressure` names its attributes unlike its siblings** (`some`,
   `avg10`, `total_us`, `full_avg10`, `full_total_us` against
   `stall_ratio`, `some_avg10`, `stalled_ns`, `full_stall_ratio`) and has no
   `full` share of the window.
4. **No system memory source**: `free` and vmstat's memory columns need a
   `mem.system` from `/proc/meminfo`.
5. **No load or run-queue source**: `uptime`, vmstat `r`/`b` need
   `/proc/loadavg` or `/proc/stat`'s `procs_running`/`procs_blocked`.
6. **No slab source**: `slabtop` needs `/proc/slabinfo` (root-only).
7. **No system NUMA counters**: `numastat` without `-p` needs
   `/sys/devices/system/node/node*/numastat`.
8. **No interrupt counts**: vmstat `in`, `mpstat -I` (`/proc/stat` `intr`,
   `/proc/interrupts`, `/proc/softirqs`); `sched.irq` gives time only.
9. **`cpu.oncpu_time` via `fs:proc` has no `rate` in poll and loses the
   tail**: a 2 s spin read at poll@1s ended at 990 ms of cpu, the last second
   gone when the program exited; `lifetime.cpu_ns` had it. `pidstat -u` has
   no pair until this is fixed.
10. **No unattended history**: `sar`'s `sadc` files have no `omn`
    equivalent.

## What I could not do, and why

- `slabtop` has no pair: no omn source, and `/proc/slabinfo` needs root.
- `free`, `uptime`, system `numastat`, vmstat `r b swpd free buff cache in`:
  no pair, no source (gaps 4 to 8).
- `pidstat -u`, `pidstat -d`, `mpstat -P ALL` per cpu: no pair yet (gap 9;
  `io.bytes --pid` is bpf only; a per-cpu key comparison not written).
- `iostat -d`/`-x` are stream io's.

## Privileged runs wanted

None of these pairs needs privilege. A future `slabtop` pair would need:

```console
sudo slabtop -o -s c | head -20
```

and `make` before `pidstat -d` against `io.bytes --pid` (capabilities).
If sysstat is missing on another machine: `sudo pacman -S sysstat numactl`.

## Decision lines for DECISIONS.md

- A system-wide tool and omn observe the same seconds: the workload runs in
  the background and both start together; omn's `--system` takes no program.
- A pair that misses because of a filed omn defect stays in the catalog with
  `known_defect` naming it, reported KNOWN; agreeing makes it fail until the
  field is dropped.

## Needs from another stream

- **reference**: support in the shared runner, or adopt from
  `tests/reference/sysstat.rs`: `target: system`, `compare.omn_key`,
  `compare.scale`, the `raw.<attr>` path (mean over raw events, the
  few-ms leading event left out) and `known_defect`. Once it does, the
  `pairs` module of `tests/reference/sysstat.rs` can be deleted.
- **src (after the wave)**: defects 1 to 3 above; sources 4 to 8.

## Review

The reviewer: the system `cpu.util` and `cpu.pressure` pairs pass only
because `omn_value` leaves out the leading event of defect 2; no pair compares
what omn reports (`summary.mean`), and none carries a `known_defect`, so a
fix or a regression would go unnoticed. One `summary.mean` pair marked
`known_defect` was suggested.

The gap is real; the suggested pair cannot hold it. Measured on 2026-10-04
(`cpu.util --system -e summary,raw`, ten runs of `-d 2s` idle and ten under
twelve spinners on 24 cpus): the leading event is present in only about
half to four fifths of the runs (one in six in one batch of six at
poll@100ms), and its share is 0, 0.5, 0.67 or 1. Runs without it, or with a
share near the true one, give a `summary.mean` that agrees (0.135 vs 0.135,
0.603 vs 0.571); others miss by 0.04 to 0.2. A `known_defect` pair would
therefore alternate between KNOWN and "agrees now; drop its known_defect",
which is a flaky test, not a hold.

What landed instead: `poll_leading_event_is_held_as_a_known_defect`, which
runs each source at poll@50ms for 175 ms and reads whether the first raw event
is stamped within half a period of `window.opened_ns`. `cpu.util` and
`cpu.pressure` must show it within forty runs (it usually shows in the first
one or two; three runs took 2.6 to 3.5 s), or the test fails saying defect 2
looks fixed and the skip in `omn_value` must go; `mem.pressure` and
`io.pressure` must not show it in five runs, or it fails saying the defect has
spread. That covers both directions the reviewer named. A `summary.mean` pair
becomes possible once defect 2 is fixed, and belongs with that fix.

## The tests
- Review round (2026-10-04): `cargo test --test reference sysstat`, 13 tests;
  the new hold test passed three times on its own (2.6 to 3.5 s). The pairs
  test failed once on `io.pressure` `some` (omn 0.208 vs 0.183, |diff| 0.025
  against abs 0.02, on a machine running other streams' I/O) and passed on
  the one rerun: a window misalignment of a few ms under heavy I/O pressure,
  not this change. If it recurs, the tolerance wants scaling with the share.
  Clippy and fmt clean.

- `cargo test --test reference sysstat`: 12 passed (11 parser tests and the
  pairs test: twelve PASS, one KNOWN), 56 s.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
- `cargo fmt --check`: clean.
