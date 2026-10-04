# `omn top`: the system view

Stream system of the nineteenth wave. The overview: the machine's cpu, GPU,
memory, power, disk and network, the target's processes beneath them, and
what all of it cost to measure. It is the first tab, so it is what `omn top`
opens on.

```console
omn top                                         # the machine, live
omn top -- ./prog                               # the machine, and prog's processes
omn top --replay tests/view_fixtures/sqlite-commits --view system
cat docs/view/gallery/system--sqlite-commits.ansi   # the snapshot, with its dimming
```

Keys: `↑`/`↓` (`j`/`k`) select a process, `enter` opens it in the ledger
view narrowed to it, `s` steps the sort (cpu, memory held, faults, time in
I/O, time waiting to run), `i` lists the devices and interfaces that moved
nothing instead of folding them into one line.

## What it draws, and the three choices behind it

**One time axis.** The machine is one box, a section per resource, and
every chart in it sits in the same columns and spans the same run: one
column is one moment, from the cpu at the top to the network at the bottom.
A disk burst and the cpu or the network that went with it line up
vertically, which side-by-side panels each scaled to their own width cannot
do. The axis under the box says the run's length and what one cell is worth
("a cell is 54.8ms, drawn at its peak"): a cell holds the largest value that
fell in it, so a spike is never averaged away, and a cell between two reads
holds the earlier read until the next one -- for a read and a half of the
reads' usual spacing, no further, so a gap of reads that did not happen
(lost, late) stays blank rather than drawn as the last value. A measured
zero is a dot on the baseline; a cell where nothing was measured (before
the first read, after the last, in such a gap) is blank.

**Where a finished run's points sit.** Live, every point is at its event's
own timestamp. A finished run's `timeline` measures its offsets from that
result's *first event*, not from the run's opening (`src/emit.rs`,
`first_ts_ns`), and `results.json` does not record when that event was. The
pollers that keep their first read as a baseline (`mem.pressure`,
`io.bytes`, `net.bytes`, `power.energy`: a ratio or a difference needs two
reads) emit one read later than those that report it (`cpu.util`, the GPU's),
so drawn from the opening they would sit 250 ms to the left of the cpu and
GPU rows. The view places each machine timeline by what the run does fix: a
poller's first round of reads is at the window's opening, and its last lands
at the window's close (`the_runner_closes_the_window` in
`src/collect/mod.rs`). The first event is therefore at the opening or one
read after it, and only one of the two puts the last read at the close; the
view takes that one. On every fixture that measured the machine this lines up every machine row's last
read within 10 ms of the cpu's. A target's timelines are not moved: its last
read is its last before it exits, which ties nothing down, and no per-task
source this view reads keeps its first read back. Core is asked (in the
handoff) to record a timeline's origin, which would make this exact.

**Colour is trust, and only trust.** The charts are drawn in the
terminal's own colour. Every coloured cell on the screen is the trust style
that `view/src/trust.rs` defines for every view: yellow for losses and late
reads, cyan for time error and low determinism, red for a refusal, magenta
for cost. A section's badges sit on its rule, on the right; a section with a
mark that dims (low determinism, a time error, losses) has its charts and
figures dimmed. On this machine most of the screen is dimmed, because most
of these sources are `low` determinism in the catalog. That is the brief's
rule applied as written. The power section is the exception (`medium`, no
badge), and it reads as the one plain figure.

**The time error drawn on the axis.** `gpu.engine_busy` is NVML's
occupancy, averaged by the driver over a window of up to a second, and
every point carries `ts_err_ns` = 1 s. The GPU section draws that second as
a cyan ruler, `├────┤`, on the run's own axis under the occupancy chart: on
a 4-second run it is half the chart wide. The fact is shown at its real
scale, not as a footnote.

The sections:

| Section | Source | What it shows |
|---|---|---|
| cpu | `cpu.util@system` | busy as a share of the whole machine, on a chart up to a round ceiling (`10.0%`, `20.0%`, `100%`); now, mean, peak; `N cpus busy, on average` from the kernel's account over the collection's window (`observed_cpu_ns / wall_ns` of the cpu result: its edges are not exactly the polls', and a short run shows it); the last read (`now` while live, `last` once the run is over), mean, peak; live, the busy time **by state** as one bar (user, sys, iowait, steal) |
| gpu | `gpu.engine_busy@system`, `gpu.mem@system` | occupancy (mean, peak), a row per engine or device when there are several, each labelled by its key, the ruler, and the memory held: against the device's size when the events carry it (`668MiB held of 11.0GiB`), otherwise `held the whole run, unmoved` rather than a full bar that would read as a full device |
| memory | `mem.pressure@system`, and `mem.rss` when the run has it | the time the machine stalled waiting for memory (`never stalled`, or `<0.01%` rather than `0.00%`), and the target's resident memory beside it |
| power | `power.energy@system` | watts from joules per read over the read's interval; mean, peak, and the joules spent. **Per domain from the events**: the perf unit reports one event per domain (`domain: package`, keyed on nothing, so the view files it under its domain, and its package past the first), sysfs one event naming every domain (`domains: package-0,psys`, each domain's joules in `domain.<name>`). **A finished run** keeps one timeline for the source: sysfs's sum of its top-level domains, drawn as `all`, or the perf unit's domains one after another at each instant, drawn as one lane each by their order (`#1`, `#2`: the names are in the events only), and not drawn at all, with the reason, when a read was missing at some instant and the order no longer says which is which |
| disk | `io.bytes@system` | bytes per device, busiest first, one scale for every row; live, bytes **read apart from written** |
| network | `net.bytes@system` | bytes per interface, the same way; live, bytes **in apart from out** |
| processes | `cpu.oncpu_time`, and every per-task source the run holds | one row per process and one per thread any source saw, with the columns that fit: cpu as a share of one cpu, its chart over the run, cpu time, RSS, faults, time waiting to run, I/O calls, time in I/O, time on locks, GPU memory. A cell no source measured for that row is `·`, never `0`. A process's main thread has the process's id; when both have a row, the thread's id is drawn `t<tid>`. Under the columns, each column's badges, once. `s` steps only through the sorts whose column is on screen |
| cost | every result the view's needs are served by | what each collector cost as a share of one cpu over its window, a bar each, the total; `≥` and dimmed where a collector's in-target cost is unknown |

A source that was refused is drawn as its reason in its section's place: on
the `machine` fixture, `cpu.oncpu_time` is refused (no collector on
`bpf:raw_tp` for a machine target), and the process list is that refusal. A
run that measured nothing of the machine (`sqlite-latency`,
`vertical-requests`, made by `omn latency`'s session and the vertical
case's) gets one paragraph naming what the session did ask for, not seven
empty frames.

**Whose results.** A machine section draws the results `omn top` labels as
the machine's (`cpu.util@system`). A session made elsewhere may label none;
the section then takes the source's results whose series name no task, and
its rule says so ("scope not recorded: taken as the machine's"), because a
cgroup's `cpu.util` would look the same. A task's result (`gpu.mem` on the
target) is never drawn as the device's.

**A short terminal.** The box needs about 28 lines at 120 columns. When it
does not fit, whole sections are left out from the last, and one line in the
loss colour counts and names them ("+5 left out, the terminal is too short:
gpu memory power disk network"); the head, the time axis and the border
stay, so what is drawn still reads on its axis.

## Live and finished

A finished run is drawn from each result's `timeline`. A live run holds
`raw` events only (`docs/view/README.md`), and the view draws those through
the same series code, with one place that knows each collector's value
attribute (`busy`, `stall_ratio`, `bytes`, `ratio`, `joules`, `ns`...). The
events say more than a timeline, and the view draws what they say: the cpu
by state, read apart from written, in apart from out, the device's memory
size, a process's name (`comm`), the RAPL domains by name. When a run kept no events for these
sources, the section's rule says what is missing ("in apart from out: in the
events only"), so its absence is read as a property of the recording.

The catalog refuses `raw` for four of these sources: `net.bytes`,
`gpu.engine_busy`, `power.energy` and `cpu.oncpu_time` (`cannot emit raw`,
measured by asking). So in a live run the network section splits in from out
and the process list names its processes, and when the run ends and
`results.json` replaces what the view held, both are gone. `cpu.util`,
`io.bytes` and `gpu.mem` do take `raw`. This view's needs do not ask for it
today, because the needs table in `docs/view/README.md` is core's and
`tests/view_model.rs` holds it to the code; the request is in the handoff.

### Shown on a recorded run with events

The fixtures carry no `raw` for these sources, so the by-state bar, the
read/written split and the device's size are not in any committed snapshot.
To see them on a real run, `omn top --dump-session -- work.sh` had `raw`
added to `cpu.util`, `io.bytes` and `gpu.mem` and was recorded with
`omn top -f s.yml --headless -o DIR` (unprivileged, while the other streams
of this wave were building on the machine and using the GPU). The replay's
machine box, drawn before this stream's review round (`now` there is
`last` today, and the power row's `package` was a guess the timeline cannot
support: it is `all` today):

```
┌ system -- the overview: cpu, memory, disk, network, GPU and power, and the processes ────────────────────────────────┐
├ cpu busy, as a share of the whole machine ───────────────────────────────────────────────────────────────── ~low det ┤
│    100% ┤                                                                        now  46.9%                          │
│         │                                                                        mean 46.8%                          │
│         │▅▅▅▅▅▅▅▅▅▅▃▃▃▃▃▃▃▃▃▃▃▃▃▃▃▃▃▃▃▃▃▃▄▄▄▄▄▄▄▄▄▄▄▆▆▆▆▆▆▆▆▆▆▆▂▂▂▂▂▂▂▂▂▂▂     ▃ peak 54.1%                          │
│         │█████████████████████████████████████████████████████████████████▆▆▆▆▆█ 11.19 cpus busy, on average         │
│       0 ┤███████████████████████████████████████████████████████████████████████                                     │
│by state  ██████████████████████████████████████████████████▓▓▓▓▓▓▓▓▓▓▓▓▓▒▒▒▒▒▒▒▒ █user 70.0% ▓sys 19.0% ▒iowait 11.0%│
├ gpu the device, as its driver says ───────────────────────────────────────────────────── ±1.00s ~low det degraded x3 ┤
│busy      ▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▆▆▆▆▆▆▆▆▆▆▆▆▆▆▆▆▆▆▆▆▆▆▆▆▆▆▆▆ 43.0% mean  44.0% peak              │
│ ≤50.0%   ███████████████████████████████████████████████████████████████████████                                     │
│when      ├─────────────────────────────────────────────────────────────────────┤ ±1.00s: the driver's sampling window│
│memory    ▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁ 668MiB held of 11.0GiB              │
├ memory time the machine stalled waiting for it ──────────────────────────────────────────────────────────── ~low det ┤
│stalled             ▁▁▁▁▁▁▁▁▁▁▁▃▃▃▃▃▃▃▃▃▃▃······································· 0.29% mean  2.02% peak              │
│target    ██████████████████████████████████████████████████████████████████      3.95MiB held, peak 3.95MiB          │
├ power watts, from the RAPL counters ─────────────────────────────────────────────────────────────────────────────────┤
│package   ███████████████████████████████████████████▇▇▇▇▇▇▇▇▇▇▇▇▇▇▇▇▇█           120W mean  127W peak  197J          │
├ disk bytes moved, per device ───────────────────────────────────────────────────────────────────── ~low det degraded ┤
│259:0               ······················▅▅▅▅▅▅▅▅▅▅▅██████████████████████▂▂▂▂▂· 1.25GiB read  0B written            │
│253:0               ▁▁▁▁▁▁▁▁▁▁▁·················································· 0B read  120KiB written             │
│          +1 moved nothing: 7:0                                                                                       │
├ network bytes moved, per interface; in apart from out: in the events only ──────────────────────── ~low det degraded ┤
│lo        ·····················▅▅▅▅▅▅▅▅▅▅▅██████████████████████▂▂▂▂▂▂▁           2.50GiB  peak 3.46GiB/s             │
```

The same run also showed what the view is not. The 1.25 GiB read and the
2.5 GiB on `lo` were not the target's: it wrote its 300 MB to a tmpfs and
read nothing from disk, and other streams were building and testing on the
machine. The launched shell's own `cpu.oncpu_time` was 0 ns while its
children did the work, because `fs:proc` reads the launched pid only. The
machine's sections are the machine's, which is what an overview is for, and
the target's share is in the process list.

## The gallery

`docs/view/gallery/system--<fixture>.txt` and `.ansi`, one per fixture, drawn
at 120x40 by `cargo test --test view_snapshots system`:

| Fixture | What to look at |
|---|---|
| `machine` | 20 s of an idle machine: the cpu's small spikes, the wifi trickle, two interfaces of seven moving, the refusal where the process list would be, NVML costing seven tenths of what the view's collectors cost |
| `cpython-hot` | one interpreter on one cpu: the cpu at 4.8 % of the machine and 95.7 % of one cpu in the process list, the 1-second GPU ruler at half the chart's width |
| `cpython-hot-heavier` | the same, twice as long |
| `http-server` | `lo` carrying 56.6 MiB under a steady cpu; four threads, three of them seen only by the per-task sched and memory sources, with `·` where nothing measured them |
| `sqlite-commits` | the disk's 28.5 MiB of commits, the cpu climbing as the commits go on, and the power section's 68.8 W against 22.7 W idle on the `machine` fixture |
| `sqlite-latency`, `vertical-requests` | runs that measured nothing of the machine: one paragraph, and what per-task data there is |

`frame--help` and `frame--narrow` are drawn on the first tab, which is this
view, so they changed with it: the help over the overview, and the overview
at 60x20, with the sections that did not fit counted on one line.

## What it does not show

- **Per cpu.** `cpu.util` is asked for the machine as one series. A per-cpu
  heat strip would need a per-cpu request, which the needs do not make.
- **Every process on the machine.** The process list is the target's tasks
  only, as the per-task sources saw them. With no target there is no list:
  the machine-wide per-task source is refused on this build.
- **Which thread belongs to which process.** `cpu.oncpu_time` on `fs:proc`,
  `mem.rss` and `gpu.mem` read a process (`/proc/<pid>`, its threads
  summed); faults, the sched sources and `io.syscall` report threads. Both
  appear as rows, the process's cpu already holds its threads', and the
  main thread is a row of its own (`t<tid>`) rather than merged into the
  process's. `enter` focuses a pid on a process row and a tid on a thread
  row.
- **The RAPL domains by name, once a run has ended**: one timeline holds
  them all, as above.
- **In apart from out, names, by state, once a run has ended**, for the
  reason above.
- **GPU engines.** NVML gives one whole-device figure (`degraded`, said on
  the rule).
- **Device names.** A disk is its `major:minor` (`259:0`), because that is
  what the series key carries; the view reads no file to name it `nvme0n1`.

**Verdict:** it shows every resource of the machine on one time axis, drawn
in the trust style with the GPU's one-second sampling error as a ruler at
its real width and each collector's price beside it, which no system monitor
does; it does not show per-cpu load, processes other than the target's, or,
once a run has ended, the splits (by state, in/out, read/written) that only
live events carry.
