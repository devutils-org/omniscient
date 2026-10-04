# The ledger: what a process costs

Stream ledger of the nineteenth wave. `omn top`'s third tab: one process's
cost sheet across every resource `omn` measures, on one screen, with what
each line cost `omn` to know and how far it can be trusted.

```console
omn top -- ./prog                                         # live, then press 3
omn top --replay tests/view_fixtures/cpython-hot --view ledger
omn top --replay A --against B --view ledger              # two runs, line for line
cat docs/view/gallery/ledger--http-server.ansi            # the snapshot, in colour
```

## What it draws

One line per resource, in this order: **cpu time**, **memory held**,
**memory faulted**, **I/O syscalls**, **retransmits**, **GPU memory**,
**GPU time**, **energy**. Each line has six columns:

| Column | What |
|---|---|
| AMOUNT | the total over the run: cpu time, peak resident bytes, faults, calls, joules |
| RATE | per second of wall-clock, a share, or what the figure is part of |
| OVER THE RUN | its shape: a sparkline (rates spread over the interval each poll covers, levels held until the next read), or the latency distribution for the syscalls, log2 bucket by bucket, from the shortest call to the longest |
| TO KNOW IT | the cpu the line's collectors spent, plus what they spent inside the target; `≥` where that second part is unknown |
| TRUST | the core trust style's badges; the amount and the rate are dimmed by the same rule |

Under a line, where the run has them:

- **per task**, the largest first, with a bar of its share, when more than
  one task holds the figure (the `http-server` fixture's faults: one task
  takes 68 % of them, the server's main task 16 %);
- under cpu time, the target's **own account** (the launched program and its
  descendants, as the scheduler counts them) and the **machine busy** time
  over the same run with the target's share of it;
- under energy, **an estimate** of the target's share, by its share of the
  machine's busy cpu time, in italics and said to be an estimate. RAPL
  counts a whole package; the catalog says per-process attribution is a
  derived estimate and must be labelled, and it is;
- the **user/system split** of the cpu and the **bytes read and written** by
  the syscalls, when the run kept the raw events that carry them -- a live
  run always does. When it did not, the line says so instead of a blank.

A source the run asked for and could not serve is drawn on its own line as
the refusal, in the words that refused it (`gpu.kernel` on every fixture). A
source the run did not ask for -- a recording made by another session -- is
said as such. Narrowed to a task (`t`, or a focus set by another view), a
line with nothing of that task says which keys its series do carry rather
than drawing a zero; the machine-wide lines stay the machine's.

Under the sheet, the **selected line's detail**: the backend that served it,
its trust marks as sentences, what the fallback said it gives up, why reads
were late, and what knowing it cost with the runner's own words for where
that figure comes from.

The last line of the sheet is **what knowing all of it cost**: the lines'
collectors' cpu summed, and as a share of what the target spent.

Keys: `↑`/`↓` select a line, `t` steps the focus through the tasks then
clears it, `enter` opens the view behind the line (cpu time to `flame`, I/O
and retransmits to `flows`, GPU time to `lanes`, memory and energy to
`system`).

## Drawn from

Only `ctx`. A finished run: each result's `summary` (count and mean, so a
sum is `mean x count`, exact for the kernel-built `io.syscall` rows since the
emit layer takes their `total_ns`), its `timeline`, its `hist`, its
`overhead`, its `losses.reasons` and `degradations`. A live run: the `raw`
events, keyed the way the summaries are, with the same sums. The sources are
the view's needs, unchanged from the table in `docs/view/README.md`.

## What the fixtures show

| Fixture | What the sheet says |
|---|---|
| `cpython-hot` | 3.72 s on cpu, 95.7 % of one cpu; own account 3.86 s, 85 % of the machine's busy time; 17.4 MiB peak, +3 MiB over the run; 3.04k faults; 82 read/write calls, 176 µs inside them; 208 J on the package, ≈177 J estimated for the target. To know it all: ≥38.6 ms, ≥1 % of the target |
| `cpython-hot` against `cpython-hot-heavier` | cpu ×2.06 and energy ×2.12 in red; memory, faults and syscalls ×1.00 and ×0.96, uncoloured: the heavier input costs cpu and energy, nothing else |
| `http-server` | 24.0k faults, two thirds in one task of four; 401 calls, 2.68 ms inside them, 97 % in the server's main task; the I/O line cost ≥58.3 ms of `omn`'s cpu to know, over twenty times the time it measured |
| `sqlite-commits` | the machine was busy 12.6 s over a 5 s run with the target at 4.52 s of it: other work held the machine, and the energy estimate (123 J of 342 J) says by how much |
| `machine` | no target: "what the machine cost"; `cpu.oncpu_time` refused in place, the GPU memory and the energy the machine's |
| `sqlite-latency`, `vertical-requests` | a session of their own made them: every line "not in this run" |

## What it does not do

- **The user/system split and the bytes are not on the recorded fixtures.**
  They are carried on the raw events only, and the ledger's needs ask
  `cpu.oncpu_time` and `io.syscall` for aggregates. The view reads them from
  raw events wherever they are; the fixtures need the needs to add `raw`
  and a re-recording (filed in `docs/fixes/w19-ledger.md`).
- **No GPU time on any fixture.** `gpu.kernel` was refused when they were
  recorded; the line, its per-kernel split and its sparkline are tested on
  nothing real yet.
- **The energy estimate is cpu time, not cycles.** It charges package energy
  by busy cpu time, without apportioning idle power or the GPU's draw.
- **Faults are a count.** The page size of each mapping is not carried, so
  no bytes are claimed.
- **One process at a time, and only the target.** The ledger reads the
  sources `omn top` asked for the target; another process on the machine
  has no sheet unless it is the target.

**Verdict:** the ledger shows what nothing else does -- one process's cost
across cpu, memory, I/O, network, GPU and energy on one screen, each line
beside what it cost `omn` to know and how far it can be trusted, and two runs
compared line for line; it does not show where inside the process the cost
went, nor any figure its sources do not carry (no byte counts from faults,
no measured per-process energy).
