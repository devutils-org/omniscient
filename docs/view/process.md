# process -- one process at a glance

Stream process of the twentieth wave. The first screen of `omn top --
<program>` (and of a recording whose results carry the program's
`lifetime`): every view's essence for that one process on one screen, a
number and a colour per tile, each tile opening the view that has the
detail. `view/src/views/process.rs`; its snapshots are
`docs/view/gallery/process--*`.

```console
omn top --replay docs/view/gallery/process-run          # the recording below; opens on process
omn top -- ./prog                                       # live
cat docs/view/gallery/process--process-run.ansi         # the snapshot, in colour
```

It asks for nothing of its own (`NEEDS` is empty): every tile reads what
another view already asks for, so adding it changed no live session.

## What is on screen

```
header   pid, and how long the program ran (from its lifetime)
verdict  first suspect: ▶ sleep 51.5% -- the cpu tile (flame) has the detail
grid     time     cpu      cost
         request  latency  threads
         wake     flows    gpu
```

Each tile: its name and the view it opens (`cpu → flame`); a headline, the
number read at a glance; one to three lines under it; and, on its last row,
the trust badges of every figure it drew. The headline is dimmed when its
trust says so; the badges are not repeated beside it, so the number stays
readable in a third of the screen.

| Tile | Opens | Headline | Read from |
|---|---|---|---|
| time | `timebar` | the first suspect and its share of the program's wall clock (`▶ sleep 51.5%`), and a 24-cell strip of the bar, each state its glyph and colour | `timebar::sheets`: the bar the timebar marks as the program's, and `omn latency`'s verdict on it, exactly as the timebar draws it |
| cpu | `flame` | the hottest own code on cpu, as caller › callee, and its share of the samples; under it, the same off cpu, by blocked time | `cpu.profile` and `sched.offcpu` stacks |
| cost | `ledger` | the program's cpu time; under it the resident peak and the page faults | the `lifetime`'s `cpu_ns` (the program's own account), else `cpu.oncpu_time`; `mem.rss`; `mem.faults` |
| request | `request`, focused on it | the slowest request, its duration, and the state it spent most in (`a001 40.2ms ▶ lock 99.8%`) | `net.request` raw events |
| latency | `heatmap` | the slowest measurement; a strip of the slowest per slice of the run; the count and the median | the first of `gpu.kernel`, `io.latency`, `net.socket`, `sched.runqlat` holding events -- the heatmap's own order, so the strip is of the map it opens on |
| threads | `lanes` | how many threads; a strip of context switches per slice | `sched.switches`, and the threads `cpu.oncpu_time` names |
| wake | `wake` | what woke the target most: a thread by name, or `hardirq` / `softirq` | `sched.wakeup`, the wake-ups whose wakee is one of the target's threads |
| flows | `flows` | bytes received and sent over sockets; under it, bytes read and written on devices | `net.socket` (`op`, `bytes`), `io.latency` (`rwbs`, `bytes`) |
| gpu | `lanes` | the target's kernels' time over the run's wall clock; the device memory peak | `gpu.kernel`, `gpu.mem` |

**The colours** say what the tool found, never a threshold:

- the time tile's headline is in the suspect state's colour (the timebar's
  palette: green on cpu, magenta run queue, red lock, blue I/O and blocked,
  grey sleep, cyan network);
- the tile whose view explains that suspect gets a thick border in the same
  colour: on cpu, sleep and blocked → `cpu` (the flame's two panes), run
  queue → `threads`, lock → `wake`, I/O and network → `flows`, child and
  other → `cost`. A tile with nothing to read is never the one marked --
  refused, not in the run, or held but empty (`no wake-up of the target`,
  `no cpu sample` with nothing off cpu either, `no switch recorded`, `no
  request ended`): the verdict line then names the suspect alone;
- a request's cause is in its state's colour; the on-cpu function in the
  on-cpu colour, the off-cpu one in the blocked colour; bytes in the
  network's and the device's;
- everything else is the trust style: dimmed when it must not be read at
  face value, badged on the tile's last row.

A tile with nothing to read says so: `not in this run` and what it needs, or
`<source> refused` and the reason in the refusing layer's words. A program
that declares no request is the common case: the request tile says `no
request declared` in dim with the refusal's reason, not in red. A detail
line is wrapped to the tile, not cut, so a reason is read in its own words;
one longer than the tile ends its last row in `…`, and the view the tile
opens draws it whole.

## Keys and the mouse

`↑`/`↓` (or `k`/`j`) move a row, `h`/`l` a tile (`←`/`→` stay the frame's,
between views), `home` the first tile, `enter` opens the selected tile's
view, narrowed to what the tile is about (the slowest request for `request`,
the bar's request for `time`, the current focus otherwise); `⌫` comes back.
A click selects a tile, a click on the selected tile opens it, a right click
comes back.

## What a stack is charged to

The cpu tile names code, not an address. A stack is charged to its two
deepest frames of the program's own code, caller first: its interpreter
frames (`py::name:file`, from `python3 -X perf`) when it has any, its
resolved frames otherwise; a frame left as `library+0x...`, a bare address
or `[unknown]` is skipped. The caller is kept because a leaf shared by every
caller -- a helper like the workload's `burn`, a system call -- does not say
whose work it was: the deepest frame alone named `burn 89%`, the pair names
`crunch › burn 71%`, which is the workload's truth (`crunch` four times
`tidy`, both through `burn`).

This is not the flame's "hottest own" (the frame a sample ends in, by
function), which on an interpreter is an interpreter's internal. Two limits:
a native program's resolved frames include the kernel's, so a profile that
ends in the kernel is charged to two kernel functions (`sqlite-commits`:
`btrfs_check_leaf › __btrfs_check_leaf`); and a program run without its
interpreter's frames (`python3` without `-X perf`, the `http-server`
fixture) is charged to the interpreter's resolved functions.

## The recording it was judged on

`docs/view/gallery/process-run/` (`results.json` and `view.json`, 2 MB),
made by `omn top` itself from the `process` workload
(`view/workloads/process/`), whose manifest declares its truth:

```console
make
target/debug/omn top --headless -d 30s -o docs/view/gallery/process-run \
  -- python3 -X perf view/workloads/process/workload.py
sed -i "s|$PWD|/omniscient|g" docs/view/gallery/process-run/*.json
```

No `-f`: the run asks what every view asks, which is what a live `omn top --
<program>` would. The snapshot `process--process-run`:

```
│first suspect: ▶ sleep 51.5% -- the cpu tile (flame) has the detail
│┌ time → timebar ──────┐┏ cpu → flame ━━━━━━━━━━━━━┓┌ cost → ledger ─────────────────┐
││▶ sleep 51.5%         │┃crunch › burn 71.0%       ┃│cpu 890ms                       │
││████████▚▚▚░░░░░░░░░░░│┃on cpu: the hottest own co┃│rss peak 46.7MiB · 12.2k faults │
││process-main (1476208)│┃off cpu: <module> › main 4┃│cpu: the program's own account  │
│┌ request → request ───┐┌ latency → heatmap ───────┐┌ threads → lanes ───────────────┐
││no request declared   ││max 926µs                 ││3 threads                       │
│┌ wake → wake ─────────┐┌ flows → flows ───────────┐┌ gpu → lanes ───────────────────┐
││hardirq ×7            ││net ↓256KiB ↑1.00MiB      ││no kernel launched              │
││                      ││disk ↓0B ↑4.07MiB         ││device memory peak 0B           │
```

Held to the manifest by `the_tiles_find_what_the_workload_did`:

| Manifest | Tile | Recorded | Verdict |
|---|---|---|---|
| `cpu`: `crunch` 0.60 s, `tidy` 0.15 s | cpu | `crunch › burn 71.0%` | right function, first; `crunch`'s share of the two is 80%, and 71% of all samples is that less the rest of the program |
| `memory`: 32 MiB held | cost | rss peak 46.7 MiB | at least the bytes, as declared |
| `socket`: 1 MiB sent, 256 KiB received | flows | `net ↓256KiB ↑1.00MiB` | exact |
| `file`: 4 MiB written | flows | `disk ↑4.07MiB` | the file's bytes reached the device (and the filesystem's own) |
| no request probe | request | `no request declared` | right, said without alarm |
| no GPU | gpu | `no kernel launched` | right |
| `sleep` 0.30 s, `lock` 0.25 s | time | `▶ sleep 51.5%` | the timebar's verdict on the main thread: sleep includes the joins and the exchange's waits, so it outweighs on cpu (0.75 s of 2.39 s) |

The lock is not the suspect here: the timebar's reading of this run does
not split the main thread's 0.25 s on the lock out of its sleep (shown by
the bar, not investigated: the run holds `sched.futex` as a summary only),
so the wake tile is not the marked one. On the `vertical-requests` fixture, where `omn latency`'s
decomposition names the lock, the time tile reads `▶ lock 99.8%` and the
request tile `a001 40.2ms ▶ lock 99.8%`.

## What it does not show

- **A live run's first screen.** A recording with a `lifetime` opens on
  `process`; a live `--pid` or launched run does not say so in its opening
  data, and opening `process` there is `src/main.rs`'s (filed to budget by
  the view stream).
- **The ledger's own cost line.** The cost tile reads the ledger's sources,
  not the ledger's sheet (private to `ledger.rs`); it shows cpu, the
  resident peak and the faults, not every line.
- **The flame's own "hottest own"**: see above; the two agree on a native
  program whose leaf is its own code.
- **Per-file bytes.** The flows tile reads devices (`io.latency`) and
  sockets; a file's bytes need `io.syscall` or `io.fs` as `raw`, which no
  view asks for by default.
