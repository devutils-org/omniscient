# `heatmap`: latency over time

Stream heatmap of the nineteenth wave. Time across, latency up on a log2
scale, density in each cell as a glyph and a colour, for any of the view's
four latency sources: `io.latency`, `sched.runqlat`, `net.socket`,
`gpu.kernel`. A histogram says how a latency is distributed and not when; a
timeline of the mean says when and flattens the distribution into one line.
The map keeps both axes, so two regimes that alternate draw as two bands, a
phase change as a band that moves, and a spike as a lone cell far above the
rest.

```console
omn top --replay tests/view_fixtures/sqlite-commits --view heatmap
omn top --replay tests/view_fixtures/sqlite-commits --headless --view heatmap
omn top -- ./prog                                 # live, then key 6
```

## What is on the screen

```
 tabs        io.latency 1.54k   sched.runqlat 1.54k in 382 rows   net.socket 0   gpu.kernel refused
 figure      the count, the range, p50 and p99, and the trust badges
 regimes     each mode of the distribution, its share, and when it was seen
 count       ▂ █ ▆ ...: how many measurements per column        the whole run
 map         ┤ the cells, latency rows labelled at powers of ten  │ the run's histogram
 axis        └┬────▲──: time from the run's start, the cursor ▲
 lost        where records were lost, as far as the run can place them
 cursor      the column under ▲: its count, p50, p99, max, and its slowest measurement
 legend      what each glyph stands for, rows per octave, the width of a column
```

| Key | What it does |
|---|---|
| `s` / `S` | the next / previous source |
| `h` `l` / `H` `L` | the cursor one / eight columns |
| `+` / `-` | zoom in time around the cursor, up to x64, and out |
| `t` | narrow every view to the slowest measurement under the cursor (its pid and tid, and its request when it was made for one) |
| `⏎` | open it: its request's waterfall when it carries a request, its thread's lane otherwise |

The first source drawn is the first of `gpu.kernel`, `io.latency`,
`net.socket`, `sched.runqlat` the run holds events of: the program's own
latencies before the scheduler's, which every run has.

## How it reads the data

- **Only `raw` events**, never the aggregated emits, so a live run draws
  exactly as a finished one (`heatmap--sqlite-commits--live-half-way`).
  A result served without `raw` -- `omn latency`'s session asks
  `sched.runqlat` as a summary -- is said as such, with the summary's figures,
  and nothing is drawn along a time it does not have (`heatmap--sqlite-latency`).
- **A kernel-bucketed row** (`sched.runqlat`, `net.socket`) stands for
  `weight` waits in one log2 bucket of one drain window. Its value is the
  bucket's low bound; the view places it at the bucket's mean, `total_ns /
  weight` -- exact when the row stands for one wait, inside the bucket always
  -- and spreads its weight over the columns its window covers, in
  proportion, since the kernel does not say when inside the window each wait
  happened. Where the window lies follows how the collector stamps the row:
  `net.socket` (`collect::io::histogram`) stamps it at the drain that closed
  the window, so the row covers `[ts - window_ns, ts]`; `sched.runqlat`
  stamps it where its first wait began (finding 44), so the row covers from
  there to its drain's close, which the view finds from the drain's other
  rows (same result, same `window_ns` to the nanosecond): at most the
  earliest first wait's end plus the window, a bound that holds every wait
  the row counted. Nothing is spread outside the run. The legend says it,
  with the typical span, and the cursor line says "alone in a 55.0ms kernel
  window" or "one of 6". A cell can therefore hold a fraction of a count,
  which the lowest glyph says (`░<1`).
- **A row of the whole run** -- `io.latency` bucketed in the kernel, a
  `weight` with no `window_ns`, stamped at the run's open -- holds no time:
  it is said ("a histogram of the whole run, built in the kernel"), never
  drawn as a burst at the start.
- **A row that is a sum** (`total_of`) has no single latency and is left out,
  with the count said.
- **Rows**: whole octaves from the smallest to the largest value, every row
  of the screen in use (2.1 to 3.7 rows per octave on the fixtures, said in
  the legend), labelled at the powers of ten; a row of its own at the bottom
  for zero, since a run queue wait of 0 is common and has no logarithm.
- **Density** is logarithmic against the fullest cell, six steps
  (`░ ▒ ▒ ▓ ▓ █`, blue to red), so one measurement shows beside a thousand.
  The glyph carries it in the plain-text snapshot, the colour in a terminal.
  The legend lists only the steps a whole count reaches.
- **Regimes**: groups of rows apart by at least one empty octave, each with
  its median, its share of what the window holds (a row half inside a zoomed
  window counts half) and when it was seen. Beyond three, the slowest and the
  two heaviest of the rest are shown and the header says how many there were
  ("5 regimes, 3 shown"): the lone spike is never the one cut. When --
  once, all through, between two moments, in bursts at a steady pace ("in
  bursts every ~121ms") or in a counted number of irregular bursts.
- **Trust**: the cells, bars and figures take `Trust::of` like every other
  view, so a low-determinism source's map is dimmed -- most of the catalog's
  traced sources are -- and its badges sit on the figure line. A refused source
  is drawn as its refusal where the map would be (`heatmap--http-server--gpu.kernel`).
- **Losses** are placed where the run can place them: a live run's evicted
  events were the oldest, so the strip shades from the start to the first
  event kept; records dropped by a collector carry no time, so the strip shades
  the whole width lightly and says "when unknown". Late rounds are said, not
  shaded: nothing is missing.

## What the fixtures show

| Snapshot | What it shows |
|---|---|
| `heatmap--sqlite-commits` | the forty commits of a full-sync sqlite run as three bands, each pulsing every ~120 ms, one pulse per commit: synchronous writes around 22 µs (76 %, mostly metadata, `WSM`), cache flushes around 480 µs (21 %, `WFSM`, 164 on each of two partitions), and 44 synchronous writes around 4.6 ms (2.8 %, `WS`); the cursor names the slowest: `sqlite3 ... WS 12.0KiB on nvme0n1p6` (the bands' operations read off the events, not drawn: the map shows the latencies and the cursor one measurement) |
| `heatmap--sqlite-commits--zoom-x8` | five commits, each a pair of 6.7 ms columns holding all three regimes: a commit's writes, its journal and its flush fall within ~13 ms of each other |
| `heatmap--sqlite-commits--live-half-way` | the same run at 2.48 s, from its events so far, badged `live` |
| `heatmap--http-server` | socket calls of a directory-listing server: one regime at ~47 µs all through, and two calls at ~435 µs and 807 µs in the drain windows from 164 ms to 320 ms only (the 807 µs one in the window that closed at 218 ms) (the first listings; that they waited on a cold directory is a guess the map cannot check) |
| `heatmap--http-server--sched.runqlat` | run queue waits, mostly 0, with the zero row of their own |
| `heatmap--vertical-requests` | the vertical case's block I/O: the reads its requests made (a002's 64 pages, a003's 32) are the ~56 µs band; the ~4.5 ms band is 32 synchronous writes no request owns, and one readahead of a002 that waited 4.70 ms -- the slowest, on which `⏎` opens request a002 |
| `heatmap--cpython-hot`, `-heavier` | a cpu-bound program has only run queue waits to show: ~7 µs, and one 175 µs wait |
| `heatmap--sqlite-latency` | a session that kept `sched.runqlat` as a summary only: said, not drawn |
| `heatmap--machine` | a run that asked none of the four: said, with how to ask |

`docs/view/drawings/heatmap--gpu-phases.txt` -- kept out of the gallery,
whose files are all held by tests -- is a hand-kept drawing of a run recorded
on this machine and not committed (14 MB), so no test holds it and it goes
stale with the view: `omn top --headless -o` on four
programs of `tests/gpu_workload.py` in a row (3000 launches of `tiny`, 200 of
`spin`, 40 of `copy` over 64 MiB, 3000 launches again). `gpu.kernel` served
6 243 operations, every one the workload issued; the map shows the four
phases as they happened -- two bursts at ~900 ns, the `spin` band at ~1.8 ms
between them, the copies at ~250 µs -- and the cursor lands on the slowest,
`[memcpy DtoH] 64.0MiB` at 6.20 ms, with the clock conversion's `±318ns`
beside the figure.

## Limits

- Losses are shaded across the whole window: the fixtures predate the run
  placing them. `results.json` now carries each drained source's drops at
  the drains that read them (`drains`: `ts_ns`, `dropped`); shading a drop
  in the column of its drain is the strip's next step, once a recording
  holds them.
- The value attribute and how a drained source stamps its rows are read
  from `results.json` (`value_attr`, `row_stamp`) when it says them; the
  view's own table (`latency_ns`, `wait_ns`, `lat_ns`, `duration_ns`; at the
  drain's close unless `sched.runqlat`) is kept for a recording made before
  it did.
- A kernel row's place in time is its window, ~100 ms on the fixtures: a
  column narrower than that smears it. Zooming in on `sched.runqlat` draws
  the window, not the moment.
- One source at a time. Two sources on one time axis (a socket's latency over
  the run queue's) would need two maps stacked, at half the rows each.

Verdict: it shows what no histogram and no mean timeline can -- that a latency's modes are separate events in time (sqlite's slow writes once per commit, a GPU program's phases, a server's first requests) and which one measurement the spike was -- but not why a band sits where it does, and for the kernel-bucketed sources its time is only as fine as their ~100 ms drain window.
