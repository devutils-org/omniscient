# The nineteenth wave, stream flows

Branch `feat/w19-flows`, from `main` at `91a0cef`. `BRIEF.md`, "flows --
where the bytes went": bytes as flows, process -> socket -> interface and
process -> file -> device, widths in proportion, each edge with its rate and
its losses. (A rate is drawn per socket or file, not per thread -> object
ribbon: a ribbon's bytes are its width.) A view: an experiment judged on screen, `docs/view/flows.md` and
its gallery.

## What landed

- **`view/src/views/flows.rs`**, the stub replaced. Two bands, each a Sankey
  drawn in half-block cells (a cell's two halves can hold two ribbons: an
  upper half block over the lower one's colour), widths in proportion to
  bytes on **one scale per band**:
  - network: thread -> socket -> *stack* -> interface;
  - storage: thread -> file (or descriptor) -> *cache* -> device.
  The target's two columns come from call rows (`net.socket`, `io.fs`, or
  `io.syscall` when `io.fs` is absent -- each as `raw`), the machine's column
  from `net.bytes` and `io.bytes` timelines. Between them, the kernel layer is
  a **junction**, not edges: no source ties a socket to an interface or a
  file's bytes to a device, so the view draws the target's bytes arriving on
  its left and the machine's leaving on its right, and says the ratio in the
  band's footer (`= 2.00x`, `= 0.13x`).
  - Out (sent, written) green above in (received, read) cyan; the machine's
    dark grey. Each socket or file has a sparkline of when its bytes moved,
    its bytes each way, and on its second row its rate, calls and the time a
    call took. Past six objects or threads, the smallest are one row.
  - Trust: each band's title carries both its sources with backend and
    badges; ribbons and figures take `Trust::style()`; a loss is a badge on
    the object it struck (its trust joined over every result its rows came
    from) and a sentence in the detail lines when it is selected; a refused
    source is
    its reason in place; a source not asked, or asked without bytes, says so
    in place while the other side of the band is drawn.
  - Keys: `↑`/`↓` (`j`/`k`) select a socket or file, the detail lines under
    the bands say all of it; `enter` returns `Action::Focus` on its pid and
    thread. A focus narrows both bands to the rows of that pid or thread.
  - Live: the rows are drawn as they come; the machine's timelines are said
    to come at the end (a live run holds a timeline only then).
  - Below a view area of 62x15 it says so in a sentence, with the size it
    has; a band too short for every node drops the smallest rows and its
    footer says how many and their bytes. Tried at 80x24,
    100x30, 120x40 and 160x50 through `omn top --headless --size`.
- **A recording of its own**, `docs/view/gallery/flows-run/` (`record.sh`
  and `data/`, 456 KB): no common fixture carries bytes per file, so the
  view is judged on a small file server (CPython's threaded `http.server`,
  an fsynced access log, fsynced uploads) under a client in another process
  that downloads files of 8 MiB, 1 MiB and 64 KiB and uploads 256 KiB,
  twelve times, over loopback. Recorded through `omn top --headless -f
  session -o` with `net.socket`, `io.syscall` and `io.fs` as `raw` and the
  two machine timelines.
- **Snapshots**: `flows--file-server` (drawn by the view's own test, from
  the recording) and the seven `flows--<fixture>` re-blessed from the stub.
- **`docs/view/flows.md`**: what it draws, what it reads, the session that
  draws it whole, what the recording shows against its ground truth, what it
  does not do, and the verdict.
- `NEEDS` is unchanged: changing it changes `docs/view/README.md`'s table,
  which `tests/view_model.rs` holds to the code and which is core's. The
  change wanted is below, under "Needs from another stream".

## Shown versus reasoned

Shown (each a test in `flows.rs`, on the recording or the fixtures):

- The three largest files are **exactly** 12 x 8 MiB, 12 x 1 MiB and
  12 x 64 KiB read (on the band); the uploads are **twelve files of exactly
  262 144 bytes** written (on the `io.fs` rows the band sums: the view draws
  them inside its "more files" row); the storage band's total is the rows'
  sum to the byte, the "more files" row included.
- One socket, its sent bytes within 1 % above the 108.75 MiB of files (the
  rest is headers), its received above the 3 MiB of uploads; **lo carried
  2.00x the socket's bytes** (held in [1.9, 2.1]): loopback counts a byte once
  sent and once received. The same 2.00x is on the `http-server` fixture
  (28.3 MiB -> 56.6 MiB).
- `enter` focuses the selected object's thread; a focus on the main thread
  takes the storage band under a tenth of its whole and empties the network
  band (the main thread never touched the socket).
- A fixture without rows (`sqlite-commits`) says "bytes per file: not in this
  run" and still draws device `259:0`.
- Half way through, live (`feed::as_live_until`): the rows are there, the
  devices are said to come at the end, and the screen says `live`.
- `allot` gives each node its proportional rows with one at least, and never
  more rows than there are.

Reasoned, not held:

- That the 0.13x of devices to files is the page cache absorbing the 110 MiB
  of reads (the files had just been written), and that the device's
  15.3 MiB against 3 MiB written is the access log's syncs and btrfs's
  metadata: the device's bytes are the machine's, reads and writes together,
  so the view says the ratio and the reason in general, not this run's
  breakdown.
- That nothing else used lo during the recording: the 2.00x says so, but a
  busier machine would make it larger, and the view would draw that too.

## What I could not do, and why

- **The socket -> interface and file -> device edges.** `net.socket` carries
  the peer's port and family, not the interface of the socket's route; `io.fs`
  carries the filesystem's superblock device (`0:28`, btrfs's anonymous
  device), and no source maps a filesystem to the block devices `io.bytes`
  counts (`259:0`). Drawn as the junction instead, and said in
  `docs/view/flows.md`. Both are collector changes in families no stream of
  this wave owns: for `STATE.md`'s "What remains".
- **A file's name.** `io.fs` keys by device and inode; the view names a file
  `btrfs 3316766`.
- **Bytes per file on the default request.** Needs the `NEEDS` change below.
- **Rx/tx per interface and reads/writes per device**: `net.bytes` and
  `io.bytes` series are keyed by interface and device only.
- **The recording among the common fixtures.** `tests/view_fixtures/` and
  `record.sh` are core's, and a new fixture there adds a snapshot to every
  view; the recording sits in the gallery instead, drawn by the view's own
  test.

## Review

One fix round, six findings, all taken:

- **"no socket or file moved a byte" on unmeasured runs** (defect). A band
  now records whether its rows were in the run (`Band::measured`); with
  nothing to select the detail line says, per band, `no socket of the
  target's moved a byte` only when the rows were there and carried none, and
  `bytes per file were not measured` otherwise. The seven fixture snapshots
  re-blessed: `sqlite-latency`, `vertical-requests` and `machine` now say
  both were not measured; `sqlite-commits`, `http-server` and the cpython
  pair say no socket moved a byte (their `net.socket` was asked as `raw`
  and held no row) and that bytes per file were not measured. Held by
  `unmeasured_bytes_are_never_said_to_be_zero`.
- **The size guard was four rows too lenient** (gap). The least inner
  height is now derived (`MIN_INNER`: summary, two bands of five rows,
  two detail lines = 13, 62x15 with the border), and the sentence says the
  size the view needs and the size it has. `draw_band` no longer returns
  silently below its least: it says so. Held by
  `below_its_least_size_the_view_says_so` (every height from 10 to 29:
  either the sentence, or both footers drawn), and tried with `omn top
  --headless --replay tests/view_fixtures/sqlite-commits --view flows
  --size 120x15` to `120x18`.
- **Nodes dropped by `allot` were not said** (gap). The footer now counts
  what each column did not draw, with its bytes, in the loss colour and
  before the caveat so a narrow screen cuts the caveat first:
  `181 files (5.41MiB) not drawn` at 80x24. Held by
  `a_node_the_rows_cannot_hold_is_counted_on_screen` (and nothing said at
  120x60).
- **The `io.syscall` `raw` fallback was untested and compared descriptors
  with devices** (gap). In that mode the band's heading and footer say
  `descriptor(s)`, the ratio is not drawn, and the caveat says why (a
  descriptor may be a socket or a pipe, whose bytes never reach a device).
  Held by `descriptors_are_not_compared_with_devices`, which drops `io.fs`
  from the recording and checks the band holds the socket's bytes too. The
  `NEEDS` request to core below stands: with it, the default `omn top` shows
  bytes per descriptor without a false files-to-devices ratio.
- **An object's trust was its first result's** (nit, taken). Each object
  records which results its rows came from and its trust is
  `Trust::joined` over them (the "more" row over the union). Held by
  `an_objects_trust_is_all_of_its_results`.
- **The handoff's wording** (nit, taken): the loss sentence is in the detail
  lines, for the selected object; the uploads are held on the rows, not the
  band; a rate is per socket or file, not per ribbon. Said above and in
  `docs/view/flows.md`.

## Privileged runs wanted

None. The recording ran on the capabilities `make` grants (the eBPF sources),
without sudo and without changing any machine state. To record it again:

```console
make && docs/view/gallery/flows-run/record.sh
OMN_VIEW_BLESS=1 cargo test -p omn-view --lib flows
```

## Decision lines for DECISIONS.md

- **flows draws the kernel layer as a junction, never as edges.** No source
  ties a socket to an interface or a file's bytes to a device; the target's
  bytes (left) and the machine's (right) are two measurements on one scale,
  and the ratio between them is drawn and said rather than an invented split.
- **A Sankey ribbon is a sum of call rows, never a multiplication.** The
  sources count in the kernel, one row per bucket; the view adds `bytes` and
  `count` and nothing else, and its totals are the rows' sums to the byte.
- **A view may carry a recording of its own in its gallery** when no common
  fixture holds what it draws (`docs/view/gallery/flows-run/`), recorded
  through `omn top -f` by a script beside it and drawn by the view's own test,
  so the common fixtures, and every other view's snapshots, do not move.

## Needs from another stream

- **core (`docs/view/README.md`, and with it `flows.rs`'s `NEEDS`)**: the
  flows row for `io.syscall` should ask `raw,summary,hist` rather than
  `summary,hist`, so the default `omn top` has bytes per descriptor. The rows
  are the same rows the kernel already drains for the summary, so the cost
  class does not change; `omn top` live keeps the last 200 000 events per
  request. The two edits must land together (`tests/view_model.rs`):
  `emit: &["raw", "summary", "hist"]` in `NEEDS` and
  `| flows | \`io.syscall\` | trace | raw,summary,hist | target | bytes per
  descriptor |` in the table. `io.fs` (per file, `cost: high`) stays a
  session option, documented in `docs/view/flows.md`. Re-recording the
  fixtures afterwards would give the storage band its files on
  `sqlite-commits` and `http-server`, and changes the other views' snapshots
  only where they read `io.syscall`.
- **core (`tests/view_fixtures/record.sh`)**, optional: adopt
  `docs/view/gallery/flows-run/record.sh`'s file server as a common fixture
  once the row above lands; the flows view's test then reads it from there.
- **for `STATE.md`'s "What remains"** (no stream owns these this wave):
  `net.socket` carrying the interface index of the socket's route; a mapping
  from a filesystem's superblock device to the block devices under it, so
  `io.fs` and `io.bytes` meet; `io.bytes` and `net.bytes` split by direction.

## The suite

While iterating: `cargo test -p omn-view --lib flows` (13 tests after the
review round) and
`cargo test --test view_snapshots flows`; clippy clean on `omn-view`.

Before the last commit, with the machine shared by every stream (load
average 37-41):

- `make check`: red on the lib tests, 1509 passed and 1 failed --
  `collect::cpu::profile::...a_profile_of_the_machine_finds_this_thread_for_the_cpu_it_burned`
  (270 samples for 199 expected), in the flaky tally; alone, it passed.
- `cargo test --workspace --no-fail-fast`: 2368 passed, 6 failed, 11
  ignored. Each failure rerun alone, once:
  - passed alone: the cpu profile test above,
    `scenario_cpu::a_core_with_one_tenant_reads_the_same_through_two_files`,
    `triage::an_escalating_run_holds_its_budget_and_names_what_it_let_go`,
    `scenario_sched::a_targets_blocking_stack_is_named_while_another_process_floods_the_stack_map`;
  - failed again: `runner::a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed`
    (4368 throttles that no ring accounts for -- the gap
    `docs/open-findings.md` already files for stream profile) and
    `cases::every_case_names_its_known_cause_both_ways` (gcc-shell's on-cpu
    6.5 s against a reference 11.6 s of cpu, under a load of 41: a cpu-time
    comparison on a saturated machine, in the tally).
  None is in `view/`, and this branch touches nothing else that runs: its
  diff is `view/src/views/flows.rs` and documents. Every `omn-view` test and
  every view snapshot passed.

After the review round (commit `86d8a17` and this one), load average 30-37:

- `make check`: red on the lib tests, 1509 passed and 1 failed -- the same
  `collect::cpu::profile::...a_profile_of_the_machine_finds_this_thread_for_the_cpu_it_burned`,
  in the flaky tally; alone, it passed.
- `cargo test --workspace --no-fail-fast`: 5 tests failed, every other
  passed. Each rerun alone, once:
  - passed alone: `family_sched::a_program_that_sleeps_a_known_time_is_off_cpu_for_that_time`,
    `scenario_cpu::a_profile_of_a_cgroup_samples_its_task_and_not_the_one_beside_it`,
    `scenario_net::two_mechanisms_measuring_one_wire_agree_to_the_link_header`,
    `unwind::a_stripped_build_is_named_from_debuginfod_only_when_asked_and_as_the_unstripped_one_is`;
  - failed again: `runner::a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed`,
    the same gap as the first round, filed for stream profile.
  None is in `view/`; the round's diff is `view/src/views/flows.rs`, the
  flows snapshots and documents. All 13 flows tests and the 15 view
  snapshot tests passed; clippy and fmt clean.
