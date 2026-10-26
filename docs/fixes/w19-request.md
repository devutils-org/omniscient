# The nineteenth wave, stream request

Branch `feat/w19-request`, from `main` at `91a0cef`. `docs/dev/BRIEF.md`, "request --
what one request costs": the same sheet as the ledger for one unit of work,
from `net.request` and the join engine: a waterfall of its life by state and
its share of each resource, held to the `http-server-*` cases' requests,
whose causes are known.

Files touched, all in the row: `view/src/views/request.rs`,
`docs/view/request.md`, `docs/view/gallery/request--*`, and this handoff.

## What landed

- **`view/src/views/request.rs`**: the view, replacing the stub. Its declared
  need is unchanged (`net.request`, trace, raw), so `docs/view/README.md`'s
  table and `tests/view_model.rs` still hold.
  - **Every request** on one row: its span (first byte in to last byte out)
    as a bar of the six states that tile it -- net in, on cpu, run queue,
    lock/sleep, disk, net out, each its own glyph and colour, so the `.txt`
    reads without colour -- **all to one scale**, and `mostly` naming the
    one or two states above a twentieth of it. `↑`/`↓` (`j`/`k`, `g`/`G`)
    select, `s` sorts slowest first, the list scrolls with the selection.
  - **The waterfall** of the selected request on its own time axis: the
    network in before `request__start` and out after `request__done` where
    they happened, the service between them filled by its states' shares
    (said under it to be shares, not an order), and **each block request it
    issued on the device when it was there**, from `io.latency` events that
    carry its `req` key.
  - **Where its time went** with the decomposition's evidence grade per state
    (`IDENTITY`/`KEY`), the lock's waker, the socket; **its share of each
    resource** (its cpu against its process's whole cpu, its device time
    against the run's, the part of its disk wait off the device from the
    `block-io` table, the network either side, its longest sleep and its
    waker); and **what sets it apart from the other requests**: each state
    against the median of the others' same state, the two furthest above.
  - `enter` focuses every view on the request; `l` opens `lanes` on its
    thread. A focus set elsewhere selects the request once; a pid or tid
    focus filters the list.
  - No request: the refusal in its own words where the view would be, or
    "this run did not ask for net.request", each followed by how to get a run
    with requests (`omn top -f examples/vertical-one-request.yml`).
  - Every figure through `trust`: request figures carry `net.request`'s marks
    (here a `degraded x2` badge, no dimming), disk figures `io.latency`'s
    (low determinism: dimmed, `~low det`), a live run `live`.
- **Tests** (`cargo test -p omn-view --lib views::request`, 11):
  each vertical request is mostly its injected cause; the six states tile
  each span; block requests go to the request whose id they carry (64 to
  a002, 32 to a003, none elsewhere, each inside its request's service) and
  their device time equals the decomposition's `parts`; what sets each
  request apart is its cause; cell allocation adds up and never overdraws;
  round ticks; the keys (select, sort keeping the selection, enter, `l`); a
  focused request is the one drawn; a refusal draws its reason and the way to
  requests; a live run half way draws the three requests ended so far, as
  `live`; small terminals (22x6 to 200x60) draw without panicking.
- **Snapshots**: `request--<fixture>` for all seven fixtures (core's harness),
  and **`request--vertical-requests--a00{1..5}`**, one per request selected
  with `↓` (`each_vertical_request_has_its_snapshot`, written through
  `snapshot::assert_snapshot`, so `OMN_VIEW_BLESS=1` rewrites them like the
  others).
- **`docs/view/request.md`**: what it draws, the five requests against their
  injected causes, what it reads and does not draw, what it does not show,
  and the verdict.

## Shown versus reasoned

Shown, on the recorded `vertical-requests` run, by the tests above and the
snapshots:

- The view names each request's injected cause, unprompted, as `mostly` and
  as "what sets it apart": a001 lock/sleep +40.1 ms (woken by tid 2759758),
  a002 disk +8.27 ms, a003 lock/sleep +20.1 ms and disk +1.97 ms, a004 net
  in +30.1 ms, a005 net out +32.7 ms.
- The block requests the view places on a request's time axis are exactly the
  ones the join engine counted (64 and 32), and their device time is the
  `block-io` table's to the nanosecond: the view and the decomposition read
  the same events and agree.
- a003's 32 block requests sit at the end of its service, after its lock
  wait; a005's network out is 28.5 ms blocked in `send` inside the service
  and 4.21 ms after `request__done`. Both are in the snapshots.
- `omn top --replay tests/view_fixtures/vertical-requests --headless --view
  request --size 100x30` draws through the real binary.

Reasoned, not shown:

- **The brief says "held to the `http-server-*` cases"**; the `http-server`
  fixture's server is `python3 -m http.server`, which fires no request
  probes, so `net.request` is refused there (core's README says the same).
  The view is held to the vertical case's server instead, whose requests'
  causes are injected and known -- the stronger ground truth. On
  `http-server` it shows the refusal, which is the correct screen.
- **Live**: shown only through `feed::as_live_until` on the recording, not
  in a terminal on a running server (no terminal in this shell; `omn top` is
  refused without one, by design).
- **Many requests**: the list scrolls and says `+N more ↓`, tested only by
  drawing the five-request fixture in 30 rows; no fixture has hundreds of
  requests.

## What I could not do, and why

- **Place the states inside a request in time.** `net.request` keeps the sums
  per state, not their order, so the service bar is shares, said so on
  screen. Filed below.
- **Declare `io.latency` as a need.** Adding it to `NEEDS` changes
  `docs/view/README.md`'s table (core's file, held by
  `tests/view_model.rs`). The view reads it when the run holds it -- the
  vertical session asks it; live `omn top` asks it for the heatmap -- and
  says so when the run lacks it. Filed below.
- **Bytes per request on the network**: not in `net.request`. Filed below.

## Privileged runs wanted

None. The view reads recordings; nothing it does needs a capability beyond
what `make` grants, and it changes no machine state.

## Decision lines for docs/DECISIONS.md

- **The request view draws the states inside a request by share, never in
  sequence**: `net.request` adds a request's time up by state in the kernel
  and keeps the sums, so an order on screen would be invented. Only what was
  timed is placed in time: the network before and after the request, and
  each block request it issued.
- **A view may read a source it did not declare, when the run holds it, and
  says where the run lacks it**: the request view reads `io.latency`'s
  `req`-keyed events beside `net.request`; its declared needs stay what it
  cannot draw without, so a run without block latency still draws every
  request.
- **What sets a request apart is measured against its siblings, not
  declared**: each state against the median of the other requests' same
  state, shown only above a twentieth of the request's span, the two
  largest. On the vertical case it names every injected cause.

## Needs from another stream

- **Core, `docs/view/README.md`**: the request row could say that the view
  also reads `io.latency`'s raw events when the run holds them (for the block
  requests each request issued, by its `req` key). Or core adds an optional
  need to `Need` (drawn when present, never asked for alone), and the request
  view declares `io.latency` through it.
- **Core, fixtures**: a recording with many requests (the vertical server
  run with a few hundred requests, mixed causes) would test the list's
  scrolling and the "apart" median on a real population rather than five.
- **The `net.request` collector's owner (coordinator: `src/bpf/request.rs`,
  `src/collect/net/request.rs`)**: two attributes would let the view draw
  more without inventing anything: the bytes received and sent per request
  (for its share of the network), and the start of its longest sleep (so the
  lock wait, the one state with a named cause, can be placed on the time
  axis). The order of all states would need a per-request log of
  transitions, which is a larger change.

## Review

One fix round on the adversarial review. Every finding was taken; none is
disputed.

- **Defect -- unknown network time drawn as known.** `Request::of` no longer
  fills in a side `net.request` left out. `first_byte` and `drained` are
  `Option`s, set only when the event carries `net_rx_ns`/`net_tx_ns` and the
  instant; an unknown side's state holds only the wait inside the service
  (`recv`/`send`), a lower bound. On screen: the span is `≥`, the heading
  says `to request__done` (`from request__start`), the waterfall marks the
  edge `?` and says "unknown after done, X in send" in the loss colour (never
  "gone when done"), no state gets a `%` or a bar of a span that is a lower
  bound (`mostly` gives times), the network line says `out ≥X` or `?`, "what
  sets it apart" says `+≥X` and takes medians over the others whose side is
  known, and the line under the summary counts the requests with a side
  unknown. Test: `an_undrained_response_is_unknown_not_gone_when_done`, on
  the vertical recording rewritten into the shape of
  `examples/vertical-abandoned.yml`'s third request (a005's `net_tx_ns`,
  `drained_ns`, `tx_drain_ns`, `span_ns` removed).
- **Gap -- `io.latency` losses beside "no block request".**
  `Gathered::blocks_unsure` applies the join engine's rule: absence is a zero
  only from an `io.latency` that lost nothing and watched the whole request
  (`window.attach_ns`/`opened_ns` to `closed_ns`). Otherwise the share
  column says "no block request seen", then "io.latency lost N, so some may
  be missing" (or "was not watching for all of it") with `io.latency`'s
  badges, and the waterfall note "none seen: io.latency lost N". Test:
  `no_block_request_under_losses_is_none_seen_with_the_loss`, which also
  holds that the clean recording still says "no block request issued".
- **Gap -- requests counted and in no event.** `Undrawn::of` reads the
  collector's notes (the run carries them as the result's `notes` or
  `degradations`): never ended, with the ids it named, how many a later
  `request__start` replaced, `request__done` with no birth, removed by the
  filter. The line under the summary says "in no event, not drawn: 1 never
  ended (req:1 in task 7, 1 replaced by a later start) · 2 ended with no
  birth". `docs/view/request.md` names `vertical-abandoned` as the run that
  has them. Test: `requests_in_no_event_are_counted_on_screen`. Reading the
  notes' wording is the only way the run carries these counts today; a
  structured field would be sturdier (filed below).
- **Nit -- the live test.** It now builds the request figures' own trust and
  asserts the summary line itself carries `live`, not the app header.
- **Nit -- the tiling test.** Compares the states' sum against `span_ns` and
  against `drained - first_byte` directly, and that the span is whole.
- **Nit -- "the other N requests".** `against_the_others` takes the visible
  (pid/tid-filtered) list.
- **Nit -- a focus the list does not hold.** Said on the line under the
  summary: "the focus names X, in no event of this run" (or "hidden by the
  pid/tid focus"), "Y is drawn instead". Test:
  `a_focus_the_list_does_not_hold_is_said`.
- **Nit -- `io.latency` under two labels.** Block events are deduplicated by
  (completion, opening, device, latency) before they are counted, in the
  request's block requests and in the run's device time. Test:
  `io_latency_under_two_labels_is_counted_once`.
- **Nit -- the share column cut on a narrow terminal.** The two columns are
  as tall as the share column's wrapped lines need (nine at least, so the
  recorded snapshots are unchanged). Test:
  `a_narrow_share_column_keeps_its_last_line`, which fails on the fixed nine
  rows (checked by reverting the height).

The seven `request--*` snapshots and the five per-request ones are unchanged:
the recorded run has every side known, nothing lost, nothing undrawn.

Still wanted, from the review: a recording of `vertical-abandoned` in
`tests/view_fixtures/` (core's directory, and every view's snapshot set grows
with it), so the loss path is held on a real run rather than a rewritten
one; and the never-ended / unborn / replaced counts as structured fields of
`net.request`'s result rather than sentences (coordinator, the collector's
owner).

## The suite, review round

While iterating: `cargo test -p omn-view --lib views::request` (17 pass),
`cargo test --test view_snapshots` (15 pass, snapshots unchanged), `cargo
clippy -p omn-view --all-targets -- -D warnings` (clean), and `cargo test
--test cli` for the documents: 25 of 26, the one red
(`a_refusal_for_an_unwritten_backend_leads_with_the_one_passed_over`, "Text
file busy" executing a binary it had just copied) passes rerun alone.

`/tmp` was over its quota on this machine during the round (26 GB of
`omn-*` test directories from every stream's runs), so the suite ran with
`TMPDIR` under the worktree's `target/`.

- `make` then `make check`: red in `omn`'s lib on
  `collect::cpu::profile::tests::a_profile_of_the_machine_finds_this_thread_for_the_cpu_it_burned`
  (a machine-wide profile, under the other streams' load); it passed in the
  run below.
- `cargo test --workspace --no-fail-fast`: **2375 passed, 8 failed, 11
  ignored**, none in `view/`. Rerun alone once: `budget::the_extra_cpu_a_faster_poll_costs...`,
  `join_engine::a_program_s_life_is_a_whole...`,
  `scenario_net::every_datagram_sent_to_a_closed_port...`,
  `scenario_proc::defect_1...`, `unwind::symbols_from_debuginfod...` and
  `unwind::dwarf_recovers_the_whole_tree...` pass;
  `runner::a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed`
  fails again (the open finding below), and
  `cases::every_case_names_its_known_cause_both_ways` (575 s, the open
  finding below) was not rerun.

## The suite, first round

While iterating: `cargo test -p omn-view --lib views::request` (11 pass),
`cargo test --test view_snapshots request`, and `cargo clippy -p omn-view
--all-targets -- -D warnings` (clean).

Before the last commit, after `make` (capabilities granted), on a machine
shared with the other streams (load average 22 to 47):

- `make check`: red in `tests/runner.rs` only --
  `a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed`
  and `a_source_attached_for_one_phase_counts_that_phase_and_nothing_of_the_other`.
  Both are in `docs/open-findings.md`. Rerun alone once: the second passes;
  the first fails again (113 951 short against its ring's 109 373), which the
  open finding "the ring-loss test is red on a quiet machine too" already
  says: throttles are not carried per ring.
- `cargo test --workspace --no-fail-fast`: **2372 passed, 5 failed, 11
  ignored**. None of the five is in `view/` or reads it:
  - `runner::a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed`
    -- the open finding above;
  - `scenario_proc::defect_1_the_first_reading_of_a_launched_target_is_of_the_program`
    and `defect_2_the_last_reading_of_a_launched_target_is_one_the_program_was_alive_for`
    -- rerun alone once: pass;
  - `unwind::dwarf_recovers_the_whole_tree_of_a_build_without_frame_pointers`
    -- rerun alone once: pass;
  - `cases::every_case_names_its_known_cause_both_ways` -- rerun alone once:
    fails again (276 s, `cpu.profile` with 13 late ticks under the load). It is
    in `docs/open-findings.md` (`http-server-stdlib`'s cost against its
    budget); it measures the machine's own cases and touches nothing of this
    branch.
- Every `view_*` test and every `omn-view` unit test passed in both runs.
