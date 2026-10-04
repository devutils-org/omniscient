# The nineteenth wave, stream lanes

Branch `feat/w19-lanes`, from `main` at `91a0cef`. `BRIEF.md`, "lanes --
threads side by side": one lane per thread, its states along time, with a lane
per GPU stream now that `gpu.kernel` has a collector, so a kernel and the host
thread that waited for it line up. Files touched: `view/src/views/lanes.rs`,
`docs/view/lanes.md`, `docs/view/gallery/lanes--*` and
`docs/view/gallery/lanes-gpu/` (the recording the view is judged on), this
file.

## What landed

- **The view** (`view/src/views/lanes.rs`), replacing the stub:
  - A thread's lane rebuilt from its **switch-outs** (`sched.switches`) and
    its **wakeups** (`sched.wakeup`): switch-out then wakeup is a block;
    wakeup to the next switch-out is awake (on a cpu or queued: `pmu:sw` has
    no switch-in); two switch-outs with no wakeup between are a preemption,
    drawn awake. Nothing is drawn before a thread's first instant or after
    its last. Without wakeups in the run, the time between switch-outs is
    `┄` and the awake share `?`, and the legend says why.
  - Each block's **state and stack** from the `sched.offcpu` row of its
    thread whose drain window holds the block's end and whose mean block is
    nearest in size (log2 distance); `exact` when the row is one block within
    an eighth of a doubling, and only then is the row's `queued_ns` said as
    that block's queueing. The detail says which kind of match it is.
  - A **GPU lane** per (process, device, stream) from `gpu.kernel`'s raw
    activities (kernels magenta, copies cyan), placed right above the
    process's main thread.
  - **Two rows** (awake above, blocked below, bars by share of the cell) for
    threads awake 1 % of the span or more, as many as the room allows; one
    row for the rest (a line where blocked, a bar where awake). Our own
    threads in the target (`omn-` prefix: the CUPTI flush thread) dim and
    italic.
  - **The detail**: the state at the cursor with its interval, the cpu left,
    the waker (thread or interrupt, and its context), the queueing, the
    stack (unnamed frames of one object folded, `libcuda.so x5`), one line of
    what every other lane was doing at that instant (grouped by state), and
    the thread's off-cpu time over the run by state and stack, from
    `sched.offcpu`'s own totals.
  - **Keys**: `↑`/`↓` `j`/`k`, `h`/`l` `H`/`L`, `[`/`]` previous/next change of
    the selected lane, `z`/`Z` zoom on the cursor, `w` to the waker of the
    block under the cursor, `enter` narrows every view (`Action::Focus`). The
    focus is honoured: a tid keeps that thread and its process's GPU lanes.
  - **The cursor opens** on the first lane's (the main thread's) longest wait
    for the GPU -- a block that ended within a tenth of an activity's length
    (or 1 ms) of its end, the activity covering four fifths of it -- else on
    the main thread's longest block.
  - Trust: the status line carries every source's count and badges; each
    lane is drawn in its sources' `Trust::style()`; a refused source's reason
    is drawn with `trust::render_refusal` under the lanes; a run with no lane
    lists what it holds of each source (absent, refused, or present without
    `raw`).
- **A recording**: `docs/view/gallery/lanes-gpu/` -- `workload.py` (a host
  program feeding a GPU: `loader`, `feeder` waiting in `cuCtxSynchronize`
  under `CU_CTX_SCHED_BLOCKING_SYNC`, `logger`; two regimes), recorded by
  `omn top --headless -o` on this machine and cut to the four sources the view
  reads (1.4 MB). Its two snapshots, `lanes--gpu-feeder` (whole run) and
  `lanes--gpu-feeder-zoom` (`zzz`, 3.4 ms a cell), are held by the view's own
  unit test (`the_recording`); the shared fixtures' seven are held by
  `tests/view_snapshots.rs lanes`, all blessed and looked at.
- **Tests** (in `lanes.rs`): block / awake / preemption from synthetic
  instants, the `sched.offcpu` match (exact and not), a run without wakeups,
  the cursor on the recording landing on the main thread's
  `cuCtxSynchronize` block over a `spin` kernel and woken by
  `cuda-EvtHandlr`, `w` and `enter`, and every key on a live half-run, a
  60x20 and a 40x8 terminal and an empty run without a panic.
- **`docs/view/lanes.md`**: what is on screen, where each glyph comes from
  and how far it holds, the recording and what the snapshots show, what the
  view does not show, and the verdict.

## Shown versus reasoned

Shown (on this machine, in the committed recording and snapshots):

- The main thread's block at the cursor (1.62 s → 1.75 s, 132 ms) lies under
  a 132 ms `spin` on stream 13; its matched `sched.offcpu` row is that block
  alone and its stack holds `cuCtxSynchronize`; it was woken by
  `cuda-EvtHandlr`, which a hardirq woke 27 µs before (read off the raw
  events of the recording, 1751.022 ms then 1751.049 ms from the run's
  opening). Held by `the_cursor_opens_on_the_main_thread_waiting_for_the_gpu`.
- The regime change: `loader` awake and `feeder` blocked on the queue for the
  first second, then `spin` filling its lane with both host threads blocked.
  Visible in `lanes--gpu-feeder.txt`; not asserted as numbers.
- Reconstruction rules, by unit test on synthetic instants.
- Nothing panics on a live half-run, small terminals, an empty run.

Reasoned, not shown:

- That a wakeup is the end of the block it follows: true of `sched_wakeup` on
  a sleeping task; a wakeup of a task already running (between
  `set_current_state` and `schedule`) lands in an awake span and is ignored
  there, which is what the code does, but no test drives a kernel into it.
- That the `sched.offcpu` match picks the right row when a window holds
  several rows of one thread near in size: the detail says it is "nearest in
  size", not exact, in that case.
- The view was not looked at in a real interactive terminal (this session
  has none): every look was through `--headless` and the snapshots. Keys are
  exercised through `App::key` in tests.

## What I could not do, and why

- **Declare `sched.wakeup` among the view's needs.** It is what turns a
  switch-out into a block, but `tests/view_model.rs` holds every need to a
  row of `docs/view/README.md`'s table, which is core's file. The view reads
  it when the run has it (`omn top` live always does: the wake view asks for
  it) and says so when it does not. Requested below.
- **Draw the shared fixtures with GPU lanes.** They were recorded before
  `gpu.kernel` had a collector, so they draw its refusal ("no collector is
  written ... yet"). Re-recording them is core's (`tests/view_fixtures/`)
  and changes every view's snapshots; the view has its own recording
  instead. Requested below.
- **Tell on-cpu from queued per instant, or name the thread that launched a
  kernel**: not in the sources (no switch-in in `pmu:sw`; `gpu.kernel` is
  keyed on the process and does not collect the launch's API record).

## The suite

`make check` once, before the handoff: red on two tests, both outside these
paths and both in the flaky tally (`docs/open-findings.md`):
`cases::every_case_names_its_known_cause_both_ways`
(`every-cited-number-agrees-with-the-reference`, on-cpu 28.0 ms apart against
26.8 ms allowed) and
`collect::cpu::profile::tests::a_profile_of_the_machine_finds_this_thread_for_the_cpu_it_burned`,
with the other streams running. Then one `cargo test --workspace
--no-fail-fast`: 2375 passed, 2 failed -- `runner::a_profile_s_losses_land_...`
(the gap filed in `docs/open-findings.md`, red on a quiet machine too) and
`unwind::user_only_stacks_carry_no_kernel_frame_and_leave_the_system_calls_out`,
which passed alone on its one rerun. Its log was `nff.log` in the scratchpad
the streams share, and another stream's run wrote the same file at the same
time (the collision `docs/open-findings.md` already describes), so those
counts are as read from a file two runs shared. `cargo clippy --workspace
--all-targets -D warnings` and `cargo fmt --check`: clean. The view's own
tests (`cargo test -p omn-view --lib lanes`, 6) and `cargo test --test
view_snapshots --test view_model` (19): green.

## Privileged runs wanted

None. The recording was made with the capabilities `make` grants
(`cap_bpf,cap_perfmon`) and nothing else; no sysctl, clock or module was
touched.

## Decision lines for DECISIONS.md

- **lanes rebuilds a thread's states from instants, never from windows**:
  blocks are switch-out → wakeup, and `sched.offcpu`'s windowed rows only
  label them (state, stack, queueing when a row is the block alone). A
  windowed total spread over a window's columns would draw a precision the
  source does not have.
- **A GPU lane lines up with host threads in time, never by attribution**:
  `gpu.kernel` is keyed on the process, so the view never says which thread
  launched a kernel.
- **The view opens on a wait for the GPU**: the cursor's first place is a
  choice of the view (the main thread's longest block that ended as an
  activity did), so a snapshot shows the lining up without a key.

## Findings

- **`sched.offcpu`'s catalog caveat says an event "is stamped at the moment
  the window's first wait began rather than at the drain that read it"; the
  `bpf:raw_tp` collector stamps it at the drain** (`ts_ns: closed_ns`,
  `src/collect/sched/offcpu.rs` `drain_into`), and the recordings agree:
  consecutive drains' `ts_ns` differ by exactly the later row's
  `window_ns`. The view reads the code's meaning (rows hold blocks that
  ended in `(ts_ns - window_ns, ts_ns]`). For the sched family's owner: the
  caveat or the stamp.
- **Every lane is dimmed on this machine**, because `sched.*` are `low`
  determinism and `gpu.kernel` carries a ±295 ns time error against cells of
  3 to 30 ms. The rule is applied as written; on this view the dimming tells
  no lane from another.

## Needs from another stream

- **core** (`docs/view/README.md`): add the row
  `| lanes | \`sched.wakeup\` | trace | raw | target | where each block ended, and who ended it |`
  to the table of needs; then `sched.wakeup` can join `lanes::NEEDS` (one
  `Need` like the wake view's: `mode: "trace"`, `emit: &["raw"]`,
  `scope: Scope::Target`) and the `WAKEUP` constant's comment goes. The
  `why` of the lanes rows changed too: `sched.switches` -- "each thread's
  switch-outs, the instant each block or preemption began"; `sched.offcpu`
  -- "the state and stack of each block, from its drain window's rows" (the
  old "each block on its own" is not what `sched.offcpu` gives).
- **core** (`tests/view_fixtures/`): a GPU fixture all views draw on --
  `docs/view/gallery/lanes-gpu/workload.py` recorded the way `record.sh`
  records the others (`omn top --headless -d 20s -o "$here/gpu-feeder" --
  python3 "$root/docs/view/gallery/lanes-gpu/workload.py"`); the ledger's
  and heatmap's `gpu.kernel` rows would then be drawn from something. Then
  `lanes--gpu-feeder` can move to the shared harness and the recording under
  the gallery can go.
- **core** (`view/src/app.rs`): the header loses its separator when the
  origin is long -- `… workload.py) │ 2.27sview: snapshot` -- on every view
  of a recording with a long command line (the lanes snapshots show it).
- **core** (`view/src/trust.rs`), to decide: a time error far below the
  resolution a figure is drawn at (±295 ns on 27 ms cells) dims it like one
  that is not; a scale-aware rule would let the dimming mean something on a
  timeline.
- **sched owner** (`catalog/sched.yaml`): the `sched.offcpu` stamping caveat,
  above.
