# process -- the system view for one process

The first screen of `omn top -- <program>` and of `--pid` (and of a recording
whose results carry the program's `lifetime`): one page that shows, at a
glance, what one process consumes of everything, one line per resource; the
detail lives in the dedicated pages, which a line opens.
`view/src/views/process.rs`; its snapshots are `docs/view/gallery/process--*`.
Redrawn in the twenty-first wave from the sketch in `BRIEF.md` ("Decided
before this wave"); the twentieth wave's tile grid is gone.

```console
omn top --replay docs/view/gallery/process-run          # the recording below; opens on process
omn top -- ./prog                                       # live
cat docs/view/gallery/process--process-run.ansi         # the snapshot, in colour
```

It asks for nothing of its own (`NEEDS` is empty): every line reads what
another view already asks for, so it changes no live session.

## What is on screen

The snapshot `process--process-run`, on the recording below:

```
 process-main  pid 1476208 · 3 threads · 2.37s · omn costs ≥10.7% cpu
──────────────────────────────────────────────────────────────────────────
 CPU     ▇████▆▆▆▄▃▂▁▁▁▂▂▂▂▅▁▁▁▁▁   0.0249 cores   avg 0.375   peak 0.993  2 late ~low det
 Memory  ▃▃▃▃▃▃▃▃▃▃▃▃▃▃▃▃▃▃████     46.7MiB        +13.7MiB/s   peak 46.7MiB  1 late
 Disk    ▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁█▁▁▁▁▁▁▁   R 0B/s         W 1.71MiB/s   p99 926µs  79 lost (67.5%) ...
 Network ▁▁▁▁▁▁▁▁▁▁▁▁▁▁▆██▂▁▁▁▁▁▁   ↓ 108KiB/s     ↑ 431KiB/s   0 retrans (machine)  ...
 GPU                                no kernel launched   VRAM 0B
 Energy  ▄▄▄▄▄▇▇▇████████████████   145 W          avg 127 W   the machine's, not apportioned  2 late
 Time    ████████▚▚▚░░░░░░░░░░░░·   sleep 51.5%   on cpu 35.1%   run queue 10.6%   I/O 0.11%  ...
──────────────────────────────────────────────────────────────────────────
▶ suspect: sleep (blocked: sleeping)  51.5%                         Enter/click → detail

CPU in one hue: the machine's cores are not in the recording
Memory in one hue: the machine's memory is not in the recording
Disk in one hue: a device's speed is not known
...
```

The sparklines cover the program's life (`lifetime`'s `released_ns` to
`exited_ns`; the run's window when there is no lifetime, now when live) in
24 cells, each cell's height against the line's own peak; a blank cell is
one nothing was read in, never a zero.

| Line | Opens | Figures | Read from |
|---|---|---|---|
| header | the cost opens `ledger` | the main thread's name, pid, threads, how long it ran, and omn's cost as the frame's corner computes it (`≥` where an eBPF collector's in-target cost is unknown) | the timebar's main bar, `cpu.oncpu_time` and `sched.switches` for the threads, `app::run_costs` |
| CPU | `flame` | cores now, the mean over the life, the peak, the share of the machine's cores | `cpu.oncpu_time`'s timeline, each poll spread over the interval it covers; the mean is the `lifetime`'s `cpu_ns` (the program's own account) over its life |
| Memory | `ledger` | resident now, its growth per second over the life, the peak, the peak's share of `MemTotal` | `mem.rss`'s timeline, carried forward between polls |
| Disk | `heatmap` (`flows` when `io.latency` is not in the run) | read and written per second, p99 of the block requests | `io.latency` raw events of the process (`rwbs`, `bytes`, `latency_ns`) |
| Network | `flows` | received and sent per second, retransmissions (the machine's, said so, when the run holds no per-process count) | `net.socket` raw events (`op`, `bytes`, each over its `window_ns`), `net.tcp.retransmit` |
| GPU | `lanes` | busy now, the mean over the life, VRAM peak | `gpu.kernel` raw events (each kernel over its `duration_ns`), `gpu.mem` |
| Energy | `system` | watts now and on average: the machine's, or, where the cores are known, the machine's by the process's share of the busy cores (`≈`, said estimated) | `power.energy` and `cpu.util` on the machine |
| Time | `timebar` | the program's wall clock split by state, the four largest with their shares | `timebar::sheets`: the bar the timebar marks as the program's |
| suspect | the page that explains the state: on cpu, sleep, blocked, lock → `flame`; run queue → `lanes`; I/O → `heatmap`; network → `flows`; else `timebar` | the state `omn latency`'s verdict names, what names it when that is a name, its share | the timebar's verdict on the same bar |

A line whose source the run did not ask says so in place and dimmed (`not
asked: needs io.latency`); a refused source says `refused` with the reason;
a source the run asked but that read nothing on the target (the machine's
results only) says `nothing on the target from cpu.oncpu_time`. None is
drawn as zero.

## The colours

**Usage is coloured by how full the resource is**: a sparkline cell and the
line's first figure take green while the resource is free, yellow past half
of it, red past four fifths -- by its share of a capacity that exists: the
cores and `MemTotal` of a live run's machine (the one the view runs on), the
GPU's engine time (busy is already a share of it). A resource with no known
capacity -- disk and network bytes on a device or link of unknown speed,
energy, and CPU and memory in a recording, which does not carry its
machine's -- keeps one neutral hue, and the lines under the suspect say
which and why.

**Trust does not wash the colour out**: a figure with a loss, a late tick, a
time error or low determinism keeps its hue, and its badges (`79 lost
(67.5%)`, `2 late`, `~low det`) stand after the line's figures.

The three steps and the neutral hue are local to `process.rs` until the
views stream's palette lands in `view/src/trust.rs`; then the hues are its.

## Keys and the mouse

`↑↓`/`jk` move between the header's cost, the seven lines and the suspect;
`Home` goes back to CPU; `Enter` opens the selected line's page (a `Goto` the
frame keeps in its history, so `⌫` or a right click comes back). A click
selects a line, a second click on it opens its page.

## The recording it was judged on

`docs/view/gallery/process-run/` (`results.json` and `view.json`, 2 MB),
made by `omn top` itself from the `process` workload
(`view/workloads/process/`), whose manifest declares its phases and its
truth:

```console
make
target/debug/omn top --headless -d 30s -o docs/view/gallery/process-run \
  -- python3 -X perf view/workloads/process/workload.py
sed -i "s|$PWD|/omniscient|g" docs/view/gallery/process-run/*.json
```

No `-f`: the run asks what every view asks, which is what a live `omn top --
<program>` would. Held to the manifest by `the_lines_find_what_the_workload_did`:

| Manifest phase | Line | Recorded | Verdict |
|---|---|---|---|
| CPU: `crunch` and `tidy`, 0.75 s on one thread | CPU | avg 0.375 over 2.37 s (0.89 s, the program's own account), peak 0.993 | the mean is the cpu burnt over the life: at least the 0.75 s; the sparkline's integral is the polled time, within 0.9× and +0.6 s; the peak at most one core and a poll's jitter |
| Memory: 32 MiB touched and held | Memory | peak 46.7 MiB, growing | at least the bytes, as declared |
| Disk: 4 MiB written and fsynced | Disk | W 1.71 MiB/s over 2.37 s (4.05 MiB) | the sparkline's integral at least the file's bytes; 79 block events lost (badged) |
| Network: 1 MiB sent, 256 KiB received | Network | ↓ 108 KiB/s, ↑ 431 KiB/s | exact: the rates are the bytes over the life, the integral within a byte |
| GPU: not exercised | GPU | `no kernel launched` | said, dimmed |
| Energy: not exercised | Energy | the machine's, not apportioned | a recording has no cores to apportion by |
| sleep 0.30 s, lock 0.25 s | Time, suspect | `sleep 51.5%` | the timebar's verdict on the main thread; the lock is not split out of the sleep (the run holds `sched.futex` as a summary only, as in the twentieth wave) |

`the_manifest_s_phases_are_the_program_s_figures` holds the manifest's
`phases` to the figures the program reads, and every line to a phase or a
`not_exercised` entry.

## What it does not show

- **A recording's capacity.** CPU and memory are coloured only live; a
  recording would need its machine's cores and memory written beside it
  (requested of run and views in `docs/fixes/w21-process.md`).
- **Disk and network fill.** A device's busy time and a link's speed are not
  read by any source the views ask for; the lines stay neutral.
- **The process's own retransmissions** where the run holds only the
  machine's: said `(machine)`.
- **The request view.** The tile grid opened it; no line here does (the
  frame's `4` does).
- **A machine run.** Opened on a recording with no target, the header says
  `no target: this run measured the machine` and the lines are every task's.
