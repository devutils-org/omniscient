# The nineteenth wave, stream core

Branch `feat/w19-core`, from `main` at `4aef377`. `BRIEF.md`, "core -- the
frame every view draws in": `omn top` live, `--replay` and `--gallery`; the
data layer; the trust style; the registry with the nine views as stubs; the
fixture recordings and the snapshot harness; and which sources each view
needs, in `docs/view/README.md`.

## What landed

- **A workspace member, `view/` (`omn-view`)**, ratatui 0.29 and its
  crossterm, linked into `omn` as the `top` subcommand. It depends on nothing
  of `omn` -- `omn` links it, and a dependency can only go one way -- so it
  reads runs through a **mirror of `results.json`** (`view/src/model.rs`),
  held to `omn`'s own types by a test (below). ratatui 0.29 rather than 0.30:
  0.30 needs rustc 1.88, above the workspace's declared 1.82.
  - `model.rs`: `Data` -- the run, the catalog's facts per source
    (determinism, summary), the refusals with their reasons, a flight
    recorder document's trigger, and `live` while a run goes. `Data::load`
    reads a `results.json`, a `trigger-<n>.json`, or a directory holding
    either, and a `view.json` beside it.
  - `trust.rs`: the one trust style. `Trust::of(data, results)` makes the
    marks (lost, late, time error, low determinism, provisional, degraded);
    losses, a time error, low determinism and a live run **dim** the figure,
    a late round and a degradation are a badge only; `error_bar` (certain
    solid, uncertain shaded), `render_refusal` (the reason where the panel
    would be), one `palette` of sixteen-colour names.
  - `views.rs`: the `View` trait, `Ctx` (data, `--against`, focus), `Focus`,
    `Action` (`Focus`, `Goto { view, focus }`), `Need`, and `registry()` with
    **all nine views registered**: `views/{system,flame,ledger,request,
    timebar,heatmap,lanes,wake,flows}.rs`, each a stub that declares its
    needs and draws `draw_stub` -- each need and what the run holds of it, in
    the trust style. A view's stream replaces its file's `draw` and touches
    nothing else.
  - `app.rs`: the frame -- header (origin, span, live badge, trigger, focus),
    tabs, help line, a help overlay with the trust legend, the cost corner,
    and the keys (`q`, `←`/`→`/`tab`, `1`-`9`, `f`, `?`; `space`/`p` in a
    gallery).
  - `cost.rs`: the view's own cpu, from `CLOCK_THREAD_CPUTIME_ID` on the
    drawing thread; the corner shows it beside the collectors' and the
    target's, each a share of one cpu over the run's wall-clock.
  - `feed.rs`: a live run's `Update`s (`Events`, `Tick`, `Ended`,
    `Restart`, and `Abort`, which closes the view and gives the terminal back
    before `omn` says why a run could not go on), a 200 000-event cap per live request with what it lets go
    counted as evicted, and `as_live_until` -- a recording as it looked live
    half way, for tests.
  - `term.rs`: the loop, drawn on `/dev/tty` so a launched program's stdout
    can be redirected without taking the screen.
  - `snapshot.rs`: a frame drawn by ratatui's test backend into text and into
    ANSI (`cat` it: the dimming is in it), the fixtures, `assert_snapshot`
    with `OMN_VIEW_BLESS=1`.
- **`omn top`** (`src/cli.rs` `TopArgs`, `src/main.rs` `top*` and `Tap`):
  - live on the machine, `--pid`, `--cgroup`, or `-- prog`; `-f session` for
    a session file's requests; `--replay RUN`, `--gallery RUN`, `--against
    RUN`, `--view NAME`; `-o DIR` writes `results.json`, `resolved.json`,
    `cost.json` and `view.json` when the run ends; `--headless` collects
    without a screen (live, needs `-o`) or prints the view -- every view in a
    gallery -- as text (`--size`); `--dump-session` prints the session a live
    run would ask for.
  - **The live session is the union of the views' needs**
    (`views::all_needs`), one request per source and scope, labelled with the
    source (`@system` for the machine-wide ones). Each need is validated
    alone, resolved alone, checked against `collect::implements`, and a need
    that fails any of the three is a **refusal with its reason**, drawn in
    place, not a refused run. A refusal the runner gives before the run starts (a
    start or an attach that failed, a target a collector cannot watch) sets
    that source aside and asks again (`Update::Restart`); a program already
    forked is killed at its gate, never run; a session file
    whose joins or decompositions would lose a request is refused whole, as
    `omn trace` refuses it, and one with `escalate:` or `record:` is refused
    with the `omn trace -f` + `--replay` route.
  - **The view watches the runner through `collect::Escalate`**, the one hook
    the runner shows a run in progress through: `Tap::decide` attaches
    nothing, hands the events read since the last hand-over to the drawing
    thread at most every 200 ms (converted through JSON into the mirror), and
    when the view is quit detaches every request ("the view was quit"), which
    ends the run as a closed window does. The runner, the collectors and
    `run_all_*` are untouched, and nothing of this runs outside `omn top`.
  - Live, each result holds its `raw` events only; the aggregates, losses,
    cost and stack names arrive with `Update::Ended`, and until then every
    figure carries the `live` mark.
- **Fixtures**, `tests/view_fixtures/<name>/{results.json,view.json}`,
  recorded through `omn top --headless -o` by
  `tests/view_fixtures/record.sh` (committed, re-runnable): `cpython-hot`,
  `cpython-hot-heavier` (twice the weights: the second run of a comparison),
  `http-server` (CPython's server and the case's client), `sqlite-commits`
  (40 synchronous commits), `sqlite-latency` (the same through `omn
  latency`'s session: the timebar's decomposition), `vertical-requests` (the
  vertical case's server through its own session: `net.request` and the two
  decompositions); and, since the review round, `machine` (20 s, no target,
  the system view's needs). 5.5 MB in all; durations in
  `docs/view/README.md`.
- **The snapshot harness**: `tests/view_snapshots.rs`, one test per view
  drawing it on every fixture against `docs/view/gallery/<view>--<fixture>.txt`
  (and `.ansi`), plus the frame's own (`frame--help`, `--gallery`, `--live`,
  `--refusal`, `--narrow`); a test that every registered view has its test.
  118 files in the gallery now, every view a stub.
- **`tests/view_model.rs`**: every fixture read by `omn`'s `RunOutput` and by
  the mirror, written back by both, compared field by field (defaults
  normalised); every fixture carries its facts and reasoned refusals; a
  recorder document written by `omn::encode::triggered_bytes` loads with its
  trigger; `docs/view/README.md`'s table of needs matches the code.
- **`tests/view_live.rs`**: a headless live run writes what `--replay` draws,
  every need served or refused with a reason; a gallery prints every view at
  the size asked; `--dump-session` is a session `omn validate` accepts; an
  unknown view, `--against` alone, a headless live run without `--out` and a
  missing recording are refused.
- **`docs/view/README.md`**: running it, the contract, finished versus live
  data, the trust table, the cost corner, the needs table, the fixtures, how a
  view is tested.

## Shown versus reasoned

Shown, on this machine with the capabilities `make` grants:

- The live path end to end, headless (`tests/view_live.rs`, and every fixture
  is one) and **on a terminal**, under `script` with a 120x40 pty: a replay
  draws, `?` opens the help, a digit moves view, `q` quits; a live run
  launched with `-d 8s` on an endless loop, quit after about two seconds of
  run, ended with every collection saying "detached 1.9 s into the run by the
  escalation: the view was quit", and `-o` wrote all four files.
- The view's own cost: the drawing thread at about 1.1-1.8 % of one cpu
  on a replay in a debug build, redrawing once a second (read off the corner
  in the pty log; the process's `/proc/<pid>/stat` agreed: 60 ms of cpu over
  five seconds). A first version showed 99 %: the first frame's draw divided
  by a few microseconds of wall, never redrawn on a still screen. The corner
  now says `measuring` for its first half second and the screen is redrawn
  once a second for it; `cost::tests` holds a sleeping and a spinning thread
  to it.
- The mirror holds every field of six real recordings (`tests/view_model.rs`);
  the test failed on its first run on `losses`, which the mirror wrote with
  its defaults where `omn` skips them -- the comparison normalises defaults
  on both sides, so a field the mirror *drops* still fails it.
- `Tap` is not what makes collectors late, **headless and on a terminal**.
  Headless, the same session run by `omn trace -f` has the same late rounds,
  and removing any one of its 17 requests does not remove them; but a
  headless run never converts anything (no receiver), so that comparison
  said nothing of the hand-over's cost. The review round measured it on a
  terminal: three pairs of the 4-second `cpython-hot` workload, each run once
  under `script` with a 120x40 pty (the view drawing, every event handed
  over) and once headless. Late rounds, summed over the run's requests:
  216/217, 210/206, 214/208 (terminal/headless). The hand-over costs cpu
  (about 110 ms of the run's 200 ms of view), and does not make the reads
  late. Filed below.

Reasoned, not measured:

- **"Unused, it costs nothing."** The only change outside `top` is the
  dispatch arm and the link: no collector, no runner path and no encoder
  changed, and `omn trace` never constructs a `Tap`. The binary is larger by
  ratatui and crossterm; its startup was not measured against `main`.
- A live run on a system target, `--pid` and `--cgroup` were run headless only
  through the code path the launch takes (they differ only in the request's
  target); not on a terminal.
- `Update::Restart` (a source set aside by the runner and the run asked
  again) is exercised by four of the six recordings (`net.request` refused at
  start: the launched program fires no request probe), headless; not watched
  on a terminal. That refusal comes from the attach, **after** the program is
  forked (the first version's comment said before): the child is parked at
  its gate, and `Gated`'s drop now kills it there rather than releasing it
  first (see Review).

## Review

An adversarial review found two defects and eight gaps; all are fixed, on
`feat/w19-core` after `2f2094b`.

- **A live run with no terminal collected for its whole duration and threw it
  away** (`term.rs`). The terminal is now a `term::Screen`, opened on the main
  thread before the session is run; where there is none, `omn top` is refused
  at once (exit 2) with `--headless -o DIR` and `--replay DIR` named, live and
  replay alike. The view raises `quit` on every way out (a guard), so a view
  that fails mid-run stops the run too. `view_live::a_live_run_with_no_terminal_is_refused_at_once`
  runs it in a session of its own, stdin closed: refused in 0.3 s, nothing
  written.
- **The view's cost was charged to the collectors** (`main.rs`). `ViewCpu`
  reads the drawing thread's cpu clock (`pthread_getcpuclockid`) and times
  the hand-over on the runner's thread; at every hand-over it samples the
  view's cpu against `getrusage(RUSAGE_SELF)`, and when the run ends takes
  the view's share out of each collection's window (and before and after
  it), keeping `attributed_cpu_ns` in proportion. `results.json`, `cost.json`,
  stderr and the corner then hold the collectors without the view; stderr
  says what was taken out, and `view.json` keeps it (`view_cpu_ns`). The
  view's figure now holds the hand-over too (`cost::Meter::start_with`).
  Measured, three terminal/headless pairs of `cpython-hot` (4 s): the view
  193-203 ms; the collectors' sum after taking it out 244-258 ms against
  227-233 headless. The 11-26 ms left (5-11 %) is not explained by the
  account -- contention, the allocator, the samples' 200 ms grain are
  candidates, none measured -- and is said in `docs/view/README.md` rather
  than subtracted. Without the fix the collectors would have read about
  450 ms, twice their cost.
- **"The Tap is not why collectors run late"** was shown headless only, where
  the Tap converts nothing. Now measured on a terminal (above): the claim
  holds, with numbers.
- **The restart comment was wrong, and the race it hid was real.** The runner
  refuses `net.request` in the attach, after the fork; the handle was dropped,
  and `Gated`'s drop released the child (exec) before freezing and killing it.
  The drop now kills a child that was never released at its gate, before its
  exec, and reaps it; a released one is ended as before. This is in
  `src/collect/launch.rs` -- outside the view, but a dropped unreleased handle
  is a program the tool never meant to run, under `omn trace` as under `omn
  top`. `launch::a_child_dropped_unreleased_never_runs_the_program` holds
  it; it could not have caught the old race (500 trials of the old drop
  never lost it -- the kill lands within microseconds), so it guards the
  guarantee rather than reproducing the defect.
- **`.ansi` was never compared.** `assert_snapshot` now compares the styles
  too, and says which file differs and that the characters agree; a missing
  `.ansi` is written and fails once, like a new snapshot. A unit test holds
  that a figure drawn without its dimming has the same text and other
  escapes.
- **An aggregated result's unknown time error was drawn as zero.** `omn top
  -o` now writes each result's worst `ts_err_ns`, from its events before
  aggregation, in `view.json`; a live run keeps it from the events handed
  over; `Results::max_ts_err_ns` is an `Option`, and `None` is a new mark,
  `TimeErrorUnknown` (`±?`, dims). It surfaced at once: `gpu.engine_busy` has
  a 1 s time error on every fixture (NVML's sampling window), which no
  snapshot showed before.
- **The collectors' figure ignored in-target cpu.** The corner adds
  `in_target_cpu_ns` where known; where any is unknown the figure is drawn
  `≥` and dimmed. `measurement_ns` is not added, on purpose: it is this
  process's cpu, already in `getrusage`'s account, and the same run-wide
  counter on every collection -- adding it would count it once per request.
- **The fixtures were too short.** Re-recorded: `cpython-hot` 4 s,
  `cpython-hot-heavier` 8 s, `http-server` 3 s (25 requests), `sqlite-commits`
  5 s (a tenth of a second of cpu between commits, no extra process or
  write), and a new **`machine`** fixture: 20 s, no target, through the
  system view's own needs (80 points per poll source). Every view's needs on
  the whole machine were 59 MB for 8 s, so the machine fixture asks only the
  system view's, read from `docs/view/README.md`'s table by `record.sh`.
  `sqlite-latency` and `vertical-requests` stay short: each is a
  decomposition, not a series. 5.5 MB in all. Recording the machine fixture
  found a bug of mine: `omn top -f` replaced a session file's own durations
  with the default ten minutes when `--duration` was not given; the file's
  durations now hold.
- **`all_needs` let the first view decide a source's mode silently.** Two
  views asking one source and scope in different modes or stacks are now
  refused, naming both (`views::merge_needs`, and its unit test); `omn top`
  fails with that reason.
- **Nit, a batch that failed to convert was lost uncounted.** A batch that
  fails is converted event by event; the ones that fail are counted as
  undecoded, with the first reason, in the live view (`Update::Events {
  unconverted }`). A NaN attribute is the test case.
- **Nit, the mirror's tests.** The mirror's `Overhead` gains `in_target_runs`
  and `in_target_left_out`. The comparison still normalises zeros and nulls
  away, so it cannot tell `Some(0)` from `None` or check a field no fixture
  holds: "holds every field of the seven recordings", no more.

## What I could not do, and why

- **No view is drawn**: that is the nine view streams' work, phase B. Every
  view is a stub drawing its needs and the run's holdings in the trust
  style.
- **No GPU fixture.** `gpu.kernel` has no collector until cupti merges, and
  running a GPU workload to record one would load the device every other
  stream is timing. Every fixture records `gpu.kernel` as refused, with the
  reason; the lanes and heatmap views need a re-recording once it lands
  (below).
- **The request view on the `http-server` cases**: the brief holds it to them,
  but `net.request` follows USDT probes the program fires, and neither CPython
  nor the shell that starts it fires any -- the source is refused on that
  fixture with that reason. The `vertical-requests` fixture (the vertical case's server, which
  fires them) is what the request view can draw.
- **The timebar's decomposition is not in the default live session**: only a
  session carries one (`omn latency --dump-session`, then `omn top -f`). The
  `sqlite-latency` fixture is that.
- **Live data is `raw` only until the end.** The runner says its aggregates,
  losses and stack names when it ends; reading them mid-run would mean a
  change to the runner (`collect/mod.rs`), which is not this stream's and
  which the brief rules out ("nothing in the collectors changes for the
  view's sake"). Views draw aggregates from events while `data.live` is set,
  or say their figure comes at the end.
- **The launched program shares the terminal's stdin** with the view: a
  program that reads its stdin competes with the keys.
- **Most figures are dimmed**: most of the catalog's traced sources are `low`
  determinism, and the brief dims low determinism. Applied as written; the
  snapshots show what it looks like, for the user to judge.

## Something I did to the shared machine

To stop my own first `make check` (started before a last edit, whose relink
would have changed the binary under test), I ran `pkill -f 'make check'`.
`-f` matches any process on the machine whose command line holds those words,
so **another stream's `make check` running at that moment would have been
killed too**; the pattern also matched the shell running it, which is why
the second `pkill` of the line never ran (a `cargo test --workspace` of the
cupti worktree was left alone). I cannot tell whether another stream had one
running then. If a stream reports a `make check` that died without a test
failure around 06:17 on 2026-10-04, this is the likely cause. The run was
started again from scratch afterwards.

## Privileged runs wanted

None that changes machine state. One recording, by the coordinator, alone on
the machine and after cupti merges, so the lanes and heatmap views have a GPU
stream to draw:

```console
make
# a fixture with GPU kernels, served by the CUPTI injection library
target/debug/omn top --headless -d 20s -o tests/view_fixtures/gpu-kernels -- \
    python3 tests/gpu_workload.py kernel 2000
# then the same rewriting record.sh's `keep` does (paths, compact JSON), and
OMN_VIEW_BLESS=1 cargo test --test view_snapshots
```

(`kernel 2000` keeps one spin kernel resident for two seconds; if the cupti
stream's `tests/gpu_workload.py` has a launch-bound workload by then, that one
gives the lanes more kernels to draw. Better still: add it to
`tests/view_fixtures/record.sh` as `record_gpu_kernels`.)

## Decision lines for DECISIONS.md

- **The view is a crate `omn` links, and it depends on nothing of `omn`**
  (wave 19): `view/` reads runs through a field-for-field mirror of
  `results.json`, held to `omn`'s types on every fixture by
  `tests/view_model.rs`. Reason: `omn` links it as `top`, and a crate cannot
  depend on the crate that links it; a mirror nobody checks drifts silently.
- **The view watches a live run through `collect::Escalate`** (wave 19): an
  escalation that attaches nothing, reads the runner's buffers between
  rounds, and detaches everything when the view is quit. Reason: it is the
  one hook the runner already shows a run in progress through, so no
  collector and no runner path changes, and a run without `omn top` never
  reaches it.
- **Live, a result is its `raw` events until the run ends; every figure is
  provisional until then** (wave 19). Reason: aggregates, losses, cost and
  stack names are what the runner says at its end; a figure drawn before is
  drawn as what it is.
- **A view's need is asked alone, and one that cannot be served is drawn as
  its refusal** (wave 19): validated, resolved, checked for a collector, and
  set aside on a runner refusal before launch, each with its reason in
  `view.json`. Reason: a source this machine lacks is not a reason to show
  nothing of the others; it is drawn where its panel would be. A session
  file whose joins or decompositions would lose a request is refused whole.
- **The trust style dims for losses, a time error, low determinism and a live
  run; a late round and a degradation are a badge only** (wave 19). Reason:
  the brief's three, plus the run not being over; a late read is a read, and a
  fallback said what it gives up.
- **The view's cost is the drawing thread's cpu, beside the collectors' and
  the target's, as shares of one cpu** (wave 19). Reason: one unit for the
  three, so they read against each other; the first half second is not
  shown, as it is the first frame and nothing else.
- **Fixtures are recorded through `omn top --headless -o` by a committed
  script, and snapshots carry an `.ansi` twin that is compared too** (wave
  19). Reason: the fixtures are real runs and can be made again; the dimming
  is the trust style, plain text cannot show it, and a twin nobody compares
  goes stale.
- **The view's cpu is taken out of the collectors' figures, not left in
  them** (wave 19): sampled against the process's account at every
  hand-over and subtracted per collection window. Reason: `getrusage` is one
  account for the whole process, and a collector must not read as costing
  what the view spent.
- **An aggregated figure's time error is unknown unless something saw its
  events** (wave 19): `omn top -o` writes it in `view.json`; elsewhere it is
  drawn `±?` and dimmed. Reason: unknown is never zero.

## Needs from another stream

- **Coordinator, `Makefile` (cupti's row)**: `make check` runs `cargo clippy
  --all-targets -- -D warnings` without `--workspace`, so `view/` (and `shim/`,
  `mcp/`) are not linted by it; the CI's clippy step is the same. `cargo
  clippy --workspace --all-targets` is clean on this branch.
- **Coordinator, the root `Cargo.toml`**: this branch adds `"view"` to
  `members` and `omn-view = { path = "view" }` to `[dependencies]`; cupti adds
  its member to the same line -- a one-line merge.
- **Coordinator, `man/omn.1`, `GUIDE.md`, `STATE.md`**: `omn top` and its
  flags are not in the manual page or the guide yet.
- **Coordinator, `docs/open-findings.md`**: a session of 17 requests (`omn top
  --dump-session`'s) makes every 10 ms-tick source late in about one round of
  ten, worst 50-110 ms, under `omn trace -f` as under `omn top`, on a terminal
  as headless; no single request is the cause (each removed in turn, the late
  rounds stay). The runner serves every request from one thread; a view that
  wants dense series pays this.
- **Coordinator, `src/collect/launch.rs`**: the review round changed
  `Gated`'s drop (an unreleased child is killed at its gate, not released
  first). A shared layer, one function; `launch`'s tests and the full suite
  ran on it.
- **Each view stream**: your file is `view/src/views/<name>.rs`; change its
  `NEEDS` as you like, but a new source is not in the fixtures until they are
  re-recorded (`tests/view_fixtures/record.sh`), which changes every
  snapshot -- ask the coordinator rather than re-recording in your branch. A
  change to `NEEDS` also changes `docs/view/README.md`'s table, which
  `tests/view_model.rs` checks: say the new row in your handoff.
- **lanes, heatmap, ledger**: `gpu.kernel` is refused on every fixture; the
  re-recording above gives them one.

## The suite

The review round, on `8e8adc2`, with the other streams sharing the machine:

- While iterating: `cargo test -p omn-view` (20), `--test view_snapshots`
  (15), `--test view_model` (4), `--test view_live` (5), `cargo test --lib
  collect::launch` (8), the two new unit tests in `src/main.rs`; `cargo
  clippy --workspace --all-targets -- -D warnings` clean.
- **`make check`**: red on its first failure, the same
  `runner::a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed`
  as before (filed as a gap red under load).
- **`cargo test --workspace --no-fail-fast`**: 2320 passed, 3 failed, 11
  ignored. That `runner` test, and two in the flaky tally
  (`docs/open-findings.md`):
  `collect::mem::page_faults::tests::sampled_in_user_mode_the_weighed_samples_are_the_faults_the_thread_took`
  and `collect::cpu::profile::tests::a_profile_of_the_machine_finds_this_thread_for_the_cpu_it_burned`;
  each rerun alone once, each passes. None is in this round's paths.

The first round, on `fde9d3a`:

- While iterating: `cargo test -p omn-view` (15), `--test view_snapshots`
  (15), `--test view_model` (4), `--test view_live` (4), all green;
  `cargo clippy --workspace --all-targets -- -D warnings` clean.
- **`make check`**: red on its first failure,
  `runner::a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed`
  -- the kernel throttled the profile 3 610 times and the rings account for
  none; `docs/open-findings.md` files it as a gap red under load, not a flake.
  Not in this stream's paths.
- **`cargo test --workspace --no-fail-fast`**: 2312 passed, 2 failed, 11
  ignored. The same `runner` test, and
  `scenario_runtime::a_probe_follows_every_thread_of_the_process` (one thread
  501 calls and another 499 where each made 500); rerun alone once, it
  passes. Every `view_*` test and the view crate's are green.
