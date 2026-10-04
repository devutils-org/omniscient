# `omn top`: the frame every view draws in

Stream core of the nineteenth wave. `omn top` is an interactive terminal view
of everything `omn` measures, live or from a recording, and nine ways of
looking at one run, each an experiment the user keeps or drops on what it
shows. This page is the guide for a view's author: what the frame hands a
view, how a figure says how far it can be trusted, which sources each view
needs, the recordings every view is drawn on, and how a view is tested.

## Running it

```console
omn top                                  # the machine, live
omn top -- ./prog args                   # a launched program, live
omn top --pid 4242 -d 1m                 # a running process
omn top -f session.yml                   # a session file's requests, live
omn top -o DIR -- ./prog                 # live, and write the run when it ends
omn top --replay DIR                     # a recorded run: results.json, a flight
                                         #   recorder's trigger-<n>.json, or a dir
omn top --gallery DIR                    # every view on one run, stepped with space
omn top --replay A --against B           # two runs, for the views that compare
omn top --replay DIR --headless --view flame   # print one view as text
omn top --gallery DIR --headless --size 120x40 # print every view as text
omn top --headless -o DIR -- ./prog      # live, no screen: collect and write
omn top --dump-session -- ./prog         # the session a live run would ask for
```

Keys: `q` quits, `←`/`→` or `tab` move between views, `1`-`9` jump, `f`
clears the focus, `?` opens the help (the keys and the trust legend). A view
adds its own keys to the help line.

A live run lasts until the view is quit, the launched program exits, or
`--duration` (10 minutes when not given, and said). Quitting ends the run;
the end of the run does not end the view, which keeps the final figures on
screen until it is quit. The screen is drawn on `/dev/tty`, so `omn top --
prog > prog.log` keeps the program's output off it. The terminal is taken
before anything is collected: where there is none to open -- an agent shell,
CI, `nohup`, `setsid` -- `omn top` is refused at once, saying to use
`--headless -o DIR` and `--replay DIR`. A session file's own durations hold
unless `--duration` is given.

## What the frame is

| Part | Where | What |
|---|---|---|
| The data | `view/src/model.rs` | `Data`: one run, as `omn` wrote it -- a mirror of `results.json`, field for field -- with the catalog's facts per source, the refusals, a recorder document's trigger, and `live` while a run goes on |
| The trust style | `view/src/trust.rs` | `Trust::of(data, results)`, its `style()` and `badges()`, `figure()`, `error_bar()`, `render_refusal()`, the `palette` |
| The views | `view/src/views.rs` | the `View` trait, `Ctx`, `Focus`, `Action`, `Need`, and `registry()` -- all nine registered |
| The frame | `view/src/app.rs` | header, tabs, help line, the cost corner, the keys, focus and navigation |
| Units | `view/src/units.rs` | `duration`, `bytes`, `count`, `percent`, `quantity(value, unit)` |
| Live | `view/src/feed.rs` | a live run's `Update`s, and `as_live_until` to test a view on a run as it looked half way |
| The loop | `view/src/term.rs` | the terminal, keys and updates |
| Snapshots | `view/src/snapshot.rs` | a view drawn into text and into ANSI, the fixtures, the comparison |
| The subcommand | `src/cli.rs` (`TopArgs`), `src/main.rs` (`top`, `Tap`) | the session, resolution, the runner, the hand-over |

### The contract

A view is one file, `view/src/views/<name>.rs`, and implements:

```rust
pub trait View {
    fn name(&self) -> &'static str;            // `--view <name>`, the tab
    fn title(&self) -> &'static str;           // one line
    fn needs(&self) -> &'static [Need];        // what it needs measured
    fn draw(&mut self, frame: &mut Frame<'_>, area: Rect, ctx: &Ctx<'_>);
    fn key(&mut self, key: KeyEvent, ctx: &Ctx<'_>) -> Action { Action::None }
    fn keys(&self) -> &'static [(&'static str, &'static str)] { &[] }
}
```

`Ctx` holds the run (`data`), the run to compare against (`against`, from
`--against`), and the `focus`: a pid, a tid or a request the user narrowed the
screen to. A key returns `Action::Focus(focus)` to narrow every view, or
`Action::Goto { view, focus }` to open another view narrowed -- from a process
in the overview into its ledger, from a request into its waterfall. The frame
takes `q`, `?`, `f`, the arrows, `tab` and the digits before a view sees a
key.

A view **reads only `ctx`**. It never opens a source, reads a file or starts
a program: that is the projection the brief settled. A view that needs data
no source gives files the request against that source's owner.

Each stub draws `views::draw_stub`, which lists the view's needs and what the
run holds of each, in the trust style. A view's stream replaces `draw` (and
adds `key`, `keys` and state as it likes) in its own file, and touches nothing
else.

### What a run holds, finished and live

A finished run -- a replay, or a live run once it has ended -- is `results.json`
whole: each request's `outputs` (the emits it asked for: `raw` events,
`timeline`, `summary`, `rate`, `hist`, `topk`, `stack`), its `losses`, its
`overhead`, the frames of its `stacks`, its `window`, `degradations` and
`notes`; and the run's `joins` and `decompositions`. `Data::for_need(label,
source)` finds the results a need is served by: the request `omn top`
labelled for it, or, in a run another session made (`omn latency`'s, a
session file's), every request of that source.

**While a live run goes, each result holds its `raw` events only.** The runner
says its aggregated emits, losses, cost and stack names when it ends, and not
before -- `omn top` reads what the runner holds between rounds, through the one
hook the runner shows a run in progress through (`collect::Escalate`), and
asks nothing else of it. Until the end every figure is provisional: `Trust`
marks it `live` and dims it. A view that draws aggregates must therefore draw
from `results.events()` while `data.live` is `Some`, or say on screen that
its figure comes at the end. `feed::as_live_until(&data, t)` turns a recording
into what it looked like live at `t`, to test exactly that. A live request
keeps its last 200 000 events on screen; what it lets go is counted as
evicted, with the reason, and dims what is left.

A source that could not be served is in `data.refused`, with the resolver's or
the runner's own words; `data.refusal(source)` finds it. The run goes on
without it: a refused panel is drawn as its reason, never as an empty panel.

### The trust style

One style, every view, never decided by a view on its own:

| Mark | Badge | Dims | When |
|---|---|---|---|
| `Lost` | `-12 lost (3.1%)` | yes | records dropped, unread, undecoded, throttled or evicted |
| `TimeError` | `±2.50µs` | yes | an event's `ts_err_ns` is non-zero: its time came from another clock |
| `TimeErrorUnknown` | `±?` | yes | nothing says the figure's time error: it holds aggregates only, from a recording without a `view.json` |
| `LowDeterminism` | `~low det` | yes | the catalog says the source is `low` determinism |
| `Provisional` | `live` | yes | the run is still going |
| `Late` | `3 late` | no | rounds of reads came late: nothing is missing, the spacing is not what was asked |
| `Degraded` | `degraded` | no | a fallback served it and said what it gives up |

A result's time error is `results.max_ts_err_ns()`: its `raw` events', or,
for a result served only as a summary, a timeline or a histogram (which do
not carry it), what `omn top` saw of its events before they were aggregated
-- written per label in `view.json` (`ts_err_ns`), and kept from a live run's
events. A recording `omn top` did not write (`omn trace -o`) has no such
file: its aggregates' time error is unknown, and drawn as unknown, never as
zero. On this machine `gpu.engine_busy` is the one source with a non-zero
time error (NVML's one-second sampling window).

`Trust::of(data, results)` makes it; `Trust::joined` for a figure several
results made. `trust.style()` is the figure's style, `trust.badges()` the spans
beside it, `trust::figure(text, &trust)` both. `trust::error_bar(value, low,
high, max, width)` draws a bar whose certain part is solid and uncertain part
shaded -- for a lower bound from a lossy run, `high` is what it would be had
nothing been lost (`Shortfall::fraction` says how much). `render_refusal`
draws a refusal into the rectangle the panel would have had. `palette` holds
the colours: sixteen-colour names only, so a terminal's theme decides what
they look like.

Most of the catalog's traced sources are `low` determinism, so most of a view
is drawn dimmed: that is the brief's rule applied as written, and the
snapshots show what it looks like.

### The cost corner

The header's right corner says, as a share of one cpu over the run's
wall-clock: the view's own cpu, the collectors' and the target's (the largest
`observed_cpu_ns` of a result on the target rather than on the machine). Live,
the last two are said at the end. A snapshot says `view: snapshot`, so it
draws the same every time.

- **The view's** is the drawing thread's cpu (`CLOCK_THREAD_CPUTIME_ID`, after
  its first half second) plus, live, the hand-over of events on the runner's
  thread (`omn` times it there, and the meter adds it).
- **The collectors'** is every result's `attributed_cpu_ns` plus what it spent
  inside its target (`in_target_cpu_ns`). Where that second part is unknown
  -- an eBPF collector without the kernel's run statistics -- the figure is a
  lower bound, drawn `≥` and dimmed. `measurement_ns` is not added: it is
  this process's cpu, already in `getrusage`'s account.
- Every collection's account is `getrusage(RUSAGE_SELF)`, the whole process,
  so a live run with a view would charge the view to every collector. `omn
  top` samples the view's cpu against the process's at every hand-over and
  takes what the view spent inside each collection's window (and before and
  after it) out of that collection's figures, in `results.json`, `cost.json`
  and on stderr. It says how much on stderr, and `view.json` keeps it
  (`view_cpu_ns`). On a 4-second cpu-bound run the view was about 200 ms
  (drawing ~90, handing over ~110); after taking it out, the collectors' sum
  was still 11-26 ms (5-11 %) above the same run headless, over three pairs
  -- what the account does not explain, said rather than hidden.

## What each view needs

`omn top` live asks for the union of these, one request per source and scope,
labelled with the source (`@system` added for a machine-wide one); `omn top
--dump-session` prints the session. The emits and keys of two views that
name one source are merged; two that ask it in different modes or with
different stacks are refused, naming both (`views::all_needs`), since one
request cannot be both. A need is validated alone,
so one the catalog refuses loses its panel, not the run. `tests/view_model.rs`
holds this table to the code.

| View | Source | Mode | Emits | Scope | What it draws from it |
|---|---|---|---|---|---|
| system | `cpu.util` | poll@250ms | timeline,summary | system | the cpu panel: utilization by state, and its sparkline |
| system | `mem.pressure` | poll@250ms | timeline,summary | system | the memory panel: stall time under memory pressure |
| system | `io.bytes` | poll@250ms | timeline,summary | system | the disk panel: bytes read and written per device |
| system | `net.bytes` | poll@250ms | timeline,summary | system | the network panel: bytes in and out per interface |
| system | `gpu.engine_busy` | poll@250ms | timeline,summary | system | the GPU panel: occupancy per engine |
| system | `gpu.mem` | poll@250ms | timeline,summary | system | the GPU panel: memory held |
| system | `power.energy` | poll@250ms | timeline,summary | system | the power panel: watts per RAPL domain |
| system | `cpu.oncpu_time` | poll@250ms | timeline,summary (keys task) | target | the process list: cpu time per thread |
| flame | `cpu.profile` | sample@997hz | stack (stack fp) | target | the on-cpu icicle |
| flame | `sched.offcpu` | trace | stack (stack fp) | target | the off-cpu icicle |
| ledger | `cpu.oncpu_time` | poll@250ms | timeline,summary (keys task) | target | cpu time, per thread |
| ledger | `mem.rss` | poll@250ms | timeline,summary | target | memory held |
| ledger | `mem.faults` | trace | summary | target | memory faulted in |
| ledger | `io.syscall` | trace | summary,hist | target | bytes read and written, and their latency as the program saw it |
| ledger | `net.tcp.retransmit` | trace | summary | system | retransmissions, the machine's: a retransmission is sent from a timer, so the collector cannot give it to a process |
| ledger | `gpu.mem` | poll@250ms | timeline,summary | target | GPU memory held |
| ledger | `gpu.kernel` | trace | raw,summary | target | GPU time per kernel |
| ledger | `power.energy` | poll@250ms | timeline,summary | system | energy, where it can be attributed |
| request | `net.request` | trace | raw | target | each request's life by state, and the network on either side of it |
| timebar | `cpu.profile` | sample@997hz | stack (stack fp) | target | on cpu |
| timebar | `sched.runqlat` | trace | raw,summary (keys task) | target | waiting to run |
| timebar | `sched.offcpu` | trace | stack (stack fp) | target | blocked, with the stack it blocked in |
| timebar | `sched.futex` | trace | summary (keys task) | target | waiting on a lock |
| heatmap | `io.latency` | trace | raw,hist | target | block request latency, of the requests the target issued |
| heatmap | `sched.runqlat` | trace | raw,summary (keys task) | target | run queue latency |
| heatmap | `net.socket` | trace | raw,hist | target | socket read and write latency |
| heatmap | `gpu.kernel` | trace | raw,summary | target | kernel durations |
| lanes | `sched.switches` | trace | raw | target | each thread's on and off cpu |
| lanes | `sched.offcpu` | trace | raw,stack (stack fp) | target | why a thread was off cpu, each block on its own |
| lanes | `gpu.kernel` | trace | raw,summary | target | a lane per GPU stream |
| wake | `sched.wakeup` | trace | raw | target | the wake chain |
| flows | `net.socket` | trace | raw,hist | target | bytes per socket |
| flows | `net.bytes` | poll@250ms | timeline,summary | system | bytes per interface |
| flows | `io.syscall` | trace | summary,hist | target | bytes per file |
| flows | `io.bytes` | poll@250ms | timeline,summary | system | bytes per device |

Two views read what no default request gives:

- **timebar** draws the triage's decomposition, which only a session carries:
  `omn latency --objective latency --dump-session -- prog > s.yml`, then `omn
  top -f s.yml`. The `sqlite-latency` fixture is that.
- **request** reads `net.request`, which follows a request between two USDT
  probes the program fires itself (`request__start(id)`, `request__done(id)`).
  CPython fires none, nor does the shell the `http-server` fixture starts it
  from, so there the source is refused with that reason; the `vertical-requests` fixture is the
  vertical case's server, which fires them, run through its session
  (`examples/vertical-one-request.yml`) with its two decompositions.

`gpu.kernel` is refused on every fixture (no collector in this build until the
cupti stream lands): that refusal is what the GPU lanes and panels draw today.

## The fixtures

`tests/view_fixtures/<name>/`: `results.json` (compact) and `view.json` (the
catalog's facts and the refusals), recorded through `omn top --headless -o`
itself by `tests/view_fixtures/record.sh`, once, and committed. Paths in them
are rewritten to `/work` and `/omniscient`.

| Fixture | Workload | What it is for |
|---|---|---|
| Fixture | Run | Workload | What it is for |
|---|---|---|---|
| `machine` | 20 s | no target: the machine, through the system view's own needs | the overview's panels and sparklines (80 points each) |
| `cpython-hot` | 4 s | `cases/cpython-interpreter/hot.py` on a 30 000-line input: one Python function is the time | on-cpu: flame, timebar, the overview |
| `cpython-hot-heavier` | 8 s | the same on twice the weights | the second run of a comparison: `--replay cpython-hot --against cpython-hot-heavier` |
| `http-server` | 3 s | `python3 -m http.server` and the case's client: 25 listings of a 15 000-entry directory on one connection | threads, sockets, a server: lanes, wake, flows, heatmap |
| `sqlite-commits` | 5 s | the `sqlite3` shell committing 40 rows one by one, rollback journal, full sync, a tenth of a second of cpu between commits | the disk over time: heatmap, flows, lanes, off-cpu |
| `sqlite-latency` | 0.4 s | the 40 commits back to back, asked through `omn latency`'s session | timebar's decomposition of one program's time |
| `vertical-requests` | 0.4 s | the vertical case's server: five requests, each with an injected cause | request, and the block I/O each request issued |

The last two are short on purpose: each is a decomposition, of one program's
time or of five requests, not a series. The `machine` fixture asks only the
system view's sources: every view's needs on the whole machine were 60 MB for
eight seconds (the traced sources see every task), and the system view draws
none of them. About 5.5 MB in all, most of it `sqlite-commits`' block requests
and wakeups (one JSON event is ~800 bytes). Re-recording changes every snapshot: each view's
test then fails until its snapshots are looked at and blessed.

## Testing a view

`tests/view_snapshots.rs` draws every view on every fixture at 120x40 and
compares it with `docs/view/gallery/<view>--<fixture>.txt`, and its styles
with `<view>--<fixture>.ansi` beside it -- the same screen with its dimming
and colours (`cat` it): a figure drawn without its trust style has the same
characters, and fails on the `.ansi`. One test per view:

```console
cargo test --test view_snapshots flame                    # compare
OMN_VIEW_BLESS=1 cargo test --test view_snapshots flame   # rewrite, once the change is meant
cat docs/view/gallery/flame--cpython-hot.ansi             # look at it
omn top --replay tests/view_fixtures/cpython-hot --view flame   # and live in a terminal
```

A snapshot that does not exist yet is written and fails once, so it is looked
at before it counts. The `frame--*` snapshots are the frame's own: the help, a
gallery, a live run half way, a refusal, a narrow terminal. A view's own unit
tests can draw with `omn_view::snapshot::draw(fixture(name), view, w, h)` and
read the text back with `snapshot::text`.

`tests/view_model.rs` holds the mirror to `omn`'s own types on every fixture
(read by both, written back by both, compared), and this page's table to the
code. `tests/view_live.rs` runs `omn top` headless live, replay, gallery and
`--dump-session`, and what it refuses.
