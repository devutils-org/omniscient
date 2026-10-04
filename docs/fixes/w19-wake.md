# The nineteenth wave, stream wake

Branch `feat/w19-wake`, from `main` at `91a0cef`. `BRIEF.md`, "wake -- who
woke whom": the wake chain from `sched.wakeup` as a tree, from a blocked
thread up to what finally unblocked it, with the time spent at each link.
Files owned: `view/src/views/wake.rs`, its snapshots under
`docs/view/gallery/wake--*`, and `docs/view/wake.md`. Nothing else was
touched.

## What landed

- **`view/src/views/wake.rs`**: the stub replaced. The view reads only
  `ctx` -- every result of `sched.wakeup` for the chain, and the other
  results' raw events for two things the source does not carry: a thread's
  name when it was only ever a waker, and which threads are the target's
  (any thread keyed in a target-scope raw source, or woken by an interrupt,
  since a target keeps an interrupt's wakeup only as its wakee; with no
  other raw source in the run, nothing is marked outside).
  - **A grid of the woken threads**, the target's first, the outside ones
    dimmed: wakeups, and what mostly woke each. Pages around the chosen
    thread; as many columns as the width holds.
  - **The chosen thread's tree.** Each wakeup is followed back -- the waker's
    latest wakeup strictly before the one it issued, and so on -- until an
    interrupt or the idle task, a thread already on the chain (`↺`), a waker
    whose own wakeup the run does not hold, or the depth (`+`/`-`, 6, at most
    12). The chains are folded into a tree; each branch carries its wakeups,
    share, the median **quiet** (the woken thread's time since its last sign
    of life in the run -- a wakeup received or issued -- an upper bound on
    how long it was blocked), the median and worst **hold** (the waker's time
    from its own wakeup to the one it issued), and a 24-cell strip of when in
    the run its wakeups came. A tree too tall drops its smallest branches
    first and says how many wakeups they held.
  - **The longest chains** under the tree, link by link with each hold;
    wakeups that one earlier wakeup led to along the same links are one line
    with their count.
  - **What the thread woke in turn**, and a legend defining quiet and hold
    and stating the source's softirq caveat in place.
  - Keys: `↑`/`↓` (`j`/`k`, PageUp/PageDown, Home) choose the thread,
    `enter` focuses it (pid and tid, every view), `l` opens it in lanes.
    A focus set elsewhere is honoured: a tid chooses the thread (again after
    a key, whenever the focused tid changes), a pid narrows the grid.
  - Trust: the frame's style throughout -- figures dimmed by `~low det`,
    badges at the head of the headline, which wraps (an 80x24 screen keeps
    them), `live` while a run goes, the refusal drawn
    in place, and a run without the source says so rather than drawing an
    empty panel.
  - The index and the folded tree are cached per state of the run (every
    result's event count, last timestamp, close), so a replay folds once and
    a live run rebuilds and refolds when events arrive. Drawing the tree is
    linear in its wakeups (the longest chains are merged through a hash map).
- **Snapshots**: `wake--<fixture>.{txt,ansi}` for the seven fixtures, and two
  of this view's own, drawn by its unit tests through the frame with a focus:
  `wake--http-server--main` (the server's main thread) and
  `wake--http-server--handler` (its handler thread) -- the default screen
  chooses the most woken thread, the client, whose chains are softirqs.
- **`docs/view/wake.md`**: what it draws, what it showed on each fixture,
  what it does not show, and the verdict.
- **Tests**, in `wake.rs` (14 after the review round): every wakeup is in its thread's tree once at
  the first link and no branch outcounts its link; the server's main-handler
  handoff is a two-link cycle with holds inside the run; interrupts end a
  chain and outside wakers are not followed; the target's threads come first;
  pid and tid focus; keys (move, depth, `l` to lanes); a run without the
  source; an 80x24 terminal keeps the tree; a live run half way draws what it
  has and is badged `live`; the two focused snapshots.

## Shown versus reasoned

Shown, on the fixtures (snapshots committed):

- `http-server`: the main thread woken 10 of 18 times by its handler, whose
  hold is 273 ms median, 501 ms worst; the longest chain
  `<- Thread-1 501ms <- python3 39.2µs ↺`; the main thread's hardirq wakeups
  after a quiet of 501 ms; the client woken 86 % from softirq.
- `sqlite-commits`: `sqlite3` woken 1 019 times, 54 % by hardirq with a
  median quiet of 378 µs; the outside wakers' chains stopping at the first
  link.
- `cpython-hot`: one wakeup in four seconds.
- The three fixtures without the source say so.

Reasoned, not shown:

- That the handler's 273 ms hold is the listing being built, and that the
  501 ms quiet is `serve_forever`'s 0.5 s poll timeout: the figures fit both,
  nothing in the run names them.
- That the hardirq quiet is the disk's service time: it is an upper bound on
  sqlite's blocked time, which is what the column says.
- That a system-scope `sched.wakeup` request draws full chains through
  outside threads: the view reads that request alone when the run holds one
  (a unit test with a synthetic `@system` result checks the counting and
  the marking), but no fixture holds a real one (privileged run below).
- That a live run of 200 000 wakeups stays responsive: the index is
  O(n log n), rebuilt and refolded on every update that brings events, and
  the drawing is linear (a unit test draws 100 000 distinct chains in about
  a second, debug build); not measured on a live run.

## What I could not do, and why

- **Run it in a terminal.** This session has no tty (`omn top` refuses one at
  once, as core designed); everything here was judged on the snapshots and
  the ANSI files. A look with `omn top --replay
  tests/view_fixtures/http-server --view wake` and `↑`/`↓` is wanted.
- **Name the thread behind a loopback softirq, or the device behind a
  hardirq.** The source does not carry either; see the request below.
- **A fixture with full chains**: it needs a privileged recording, and the
  fixtures are core's.

## Privileged runs wanted

A fixture whose chains run through threads outside the target, to judge the
tree deeper than two links. Untested session; `make` first for the
capabilities:

```console
cat > /tmp/wake-system.yml <<'EOF'
requests:
  - label: sched.wakeup@system
    source: sched.wakeup
    target: system
    mode: trace
    emit: [raw]
  - label: sched.switches
    source: sched.switches
    target:
      launch: [sh, -c, "python3 cases/http-server-stdlib/client.py /tmp/www /big/ 25 >/dev/null & exec python3 -m http.server -p HTTP/1.1 --bind 127.0.0.1 --directory /tmp/www 0"]
    mode: trace
    emit: [raw]
EOF
target/debug/omn top --headless -d 10s -f /tmp/wake-system.yml -o /tmp/wake-system
target/debug/omn top --replay /tmp/wake-system --headless --view wake --size 120x40
```

The label matters: the view tells a machine-wide request by its
`@system` suffix, as `omn top` labels one. (`/tmp/www` prepared as `tests/view_fixtures/record.sh`'s `record_http_server`
prepares `$work/www`.) If it is worth keeping, core's `record.sh` takes it.

## Decision lines for DECISIONS.md

- **The wake view follows a chain through each waker's latest wakeup before
  the one it issued, and folds the chains per thread into a tree** (wave 19).
  Reason: `sched.wakeup` gives one link per event; the waker's own previous
  wakeup is the only link to the one before it the source holds, and folding
  is what makes a thousand wakeups readable, as folding stacks does for
  samples.
- **Two times per link, both from the wakeups alone: the waker's hold and
  the woken thread's quiet, the second drawn as an upper bound** (wave 19).
  Reason: the view is a projection of one source; exact blocked time is the
  off-cpu sources', drawn by flame and lanes, and a bound labelled as one is
  honest where a guess is not.
- **A chain stops, and says why, at an interrupt, a cycle, an unseen waker
  or the depth** (wave 19). Reason: an unseen waker on a launch target is
  the source's scope, not an absence of cause; drawing it as a stop with its
  reason keeps it from reading as the origin.

## Needs from another stream

- **core** (`docs/view/README.md`): nothing required; the table of needs is
  unchanged. If the help overlay ever shows a view's own legend, the wake
  view's four-line legend would move there and give the tree its rows.
- **core** (fixtures): optionally, the system-scope recording above as a
  fixture.
- **The sched family's owner** (`sched.wakeup`, not this wave): two fields
  would let the view name what it now says it cannot -- the irq number on a
  `hardirq` wakeup, and on a `softirq` one served in a task's
  `local_bh_enable`, that task as `served_in_tid` (the caveat already says
  the sender is the cause). Filed as a want, not a defect.

## Review

The adversarial review's eight findings, all taken:

- **Quadratic merge of the longest chains** (defect): merged through a
  `HashMap` keyed on the origin and the wakers; `a_hundred_thousand_distinct_chains_draw_in_one_pass`
  draws a producer and worker's 100 000 distinct chains.
- **Badges clipped at 80x24** (defect): the headline now leads with the
  badges and wraps over its two rows; `a_narrow_terminal_still_says_what_the_run_lost`
  sets a loss on the fixture and finds it at 80x24.
- **The live test proved nothing** (gap): it now checks the view's own
  headline starts with `~low det live`, and that the frame's line holding
  the wakeup count carries `live`.
- **A key press pinned the thread against a later focus** (gap): the chosen
  tid is kept with the focused tid it was chosen under, and a different
  focus chooses again (`a_focus_set_after_a_key_chooses_the_thread_again`).
  Left: a focus set back to the very tid it was before is not seen as a
  change; the trait has no focus hook to tell it.
- **System scope** (gap): a `@system` request is read alone when present
  (no double count, trust joined over what is read), and an interrupt's
  wakee is marked the target's only from a target-scope request
  (`a_machine_wide_request_is_read_alone_and_marks_no_interrupt_wakee`).
  The privileged session's label is now `sched.wakeup@system`.
- **Quiet's direction** (nit): the legend, the module doc, `wake.md` and
  this report now say upper bound -- blocked no longer than the quiet.
- **A merged chain drew its smallest hold; a return to the root read as
  unseen** (nit): the merged line draws the group's longest; the cycle check
  runs before the unseen one, so the handler's `+65.0ms` chain now ends `↺`.
- **Stamp counted only wakeups** (nit): it counts every result's events.

Snapshots re-blessed: every `wake--*` (the headline's order, the legend's
words, the two chain fixes); no other view's.

## The suite

- While iterating: `cargo test -p omn-view --lib wake` (10) and
  `cargo test --test view_snapshots wake` (1), green; `cargo clippy -p
  omn-view --all-targets` clean.
- **`make check`** (on `d321385`, load average 23 to 33 with the other
  streams on the machine): red on its first failures,
  `collect::cpu::profile::tests::a_profile_of_the_machine_finds_this_thread_for_the_cpu_it_burned`
  and
  `budget::a_traced_series_over_its_ceiling_counts_what_the_task_did_and_moves_only_onto_weighed_samples`,
  both in the flaky tally (`docs/open-findings.md`); each rerun alone once,
  each passes.
- **`cargo test --workspace --no-fail-fast`**: 2364 passed, 8 failed, 11
  ignored, at the same load. The profile test again, and seven integration
  tests that time or count a real run of `omn`:
  `cases::every_case_names_its_known_cause_both_ways` (an on-cpu figure 82 ms
  off its reference, a suspect named `cpu-queue` where the known cause is
  on-cpu -- run-queue wait under load),
  `scenario_net::every_datagram_sent_to_a_closed_port_is_a_drop_with_one_reason`,
  `scenario_proc::defect_1_the_first_reading_of_a_launched_target_is_of_the_program`,
  `triage::a_layer_this_run_cannot_serve_is_reported_and_the_cheap_answer_stands`,
  `triage::an_input_this_machine_cannot_serve_is_dropped_by_name_or_fails_the_question`,
  `runner::a_memory_limit_turns_a_cached_read_into_device_reads_and_each_byte_carries_its_rung`,
  and `walkthrough::the_walk_through_hands_over_what_the_runs_wrote_and_what_the_program_did`.
  None reaches this stream's code: the wake view is drawn only by `omn top`,
  and this branch changes nothing else. Not investigated; the merge's own
  full run, on a quieter machine, is the one that says whether any is real.
- `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt
  --check`: clean (run on their own, since `make check` stops at the tests).

### The review round's suite

- While iterating: `cargo test -p omn-view --lib wake` (14) and
  `cargo test --test view_snapshots` (15, every `wake--*` re-blessed and no
  other view's), green; `cargo clippy --workspace --all-targets -D warnings`
  and `cargo fmt --check` clean.
- **`make check`** on `47fc6d9`: red on one failure in `omn --lib`,
  `collect::mem::page_faults::tests::sampled_in_user_mode_the_weighed_samples_are_the_faults_the_thread_took`;
  rerun alone once, it passes.
- **`cargo test --workspace --no-fail-fast`** at a load average of 37: 2368
  passed, 13 failed, 11 ignored. The page-fault test passed this time. The 13
  are all integration tests that launch and measure a real `omn` run, in
  `family_mem`, `runner`, `scenario_gpu`, `scenario_io`, `scenario_net`,
  `scenario_proc` and `triage`. None of them goes through `omn top`, the only
  thing that draws this view, and this round changes nothing else. `omn_view`
  (34), `view_snapshots` (15) and `cli` (26) are all green. I did not
  investigate the 13; the merge's full run on a quieter machine will show
  whether any of them is real.
