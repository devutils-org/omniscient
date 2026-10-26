# The twentieth wave, stream record

Item 2 of `docs/dev/STATE.md`'s "What remains", phase B's recording half: one
re-recording of every fixture on merged phase A, a recording of each view's
workload, every snapshot redrawn, each view held to its workload's manifest,
and the gallery's README rewritten with the user's verdict. Branch
`feat/w20-record`, on `main` at `777d6dd` (every phase A stream merged).

## What landed

**One re-recording** (`tests/view_fixtures/record.sh`, `7cf6e93`), through
`omn top --headless` with every view's needs, on a binary carrying the
capabilities (`make`), in 2 min 37 s:

- The seven realistic runs again. `cpython-hot` and `cpython-hot-heavier` now
  run under `-X perf`, so their Python functions are named through the
  interpreter's perf map (the "CPython run with its perf map").
- New beside them: `file-serving` (the file server of
  `docs/view/gallery/flows-run/`, whose `record.sh` now takes an output
  directory), `vertical-abandoned` (`examples/vertical-abandoned.yml`),
  `many-requests` (300 requests on the vertical server, four causes in turn;
  the server is built from `examples/vertical/server.c` with its 64-entry plan
  limit raised by `sed`, into `target/`, the source untouched) and
  `system-wakeup` (every wakeup on the machine for one second; five seconds
  was 13 MB, one is 2.2 MB).
- A recording per view's workload, `workload-<view>` for the nine views,
  launched as each manifest's `run` says from the repository's root (the
  request server built first, its plan read from its session). `workload-lanes`
  is the GPU fixture: twelve `spin` kernels on the device, served by
  `vendor:cupti`. `process`'s workload is not recorded here: the process
  stream records it into `docs/view/gallery/process-run/`.
- 20 fixtures, 22 MB in all (`many-requests` 4.6 MB, `sqlite-commits`
  3.3 MB, `system-wakeup` 2.2 MB the largest).

**Every snapshot redrawn** on them (`OMN_VIEW_BLESS=1 cargo test --test
view_snapshots`, and the views' own tests with the same variable), except
`process--*`, which is the process stream's: the existing ones are left as
they were and the new fixtures have none yet (see "Not done").

**`tests/view_truth.rs`** (new, `92256d1`): each of the nine views drawn on
its own workload's recording, the manifest's numbers read off the screen the
way a user reads them, within each figure's `trust`:

| View | What is held | Drawn on this recording |
|---|---|---|
| flame | `heavy`/`light` and `heavy`/`medium` within 20 % of 4 and 2, through the view's own search; `nap` off cpu at least 0.60 s | 53.9 %, 15.0 %, 28.6 % (3.59, 1.88); `nap` 23.0 % of 2.61 s = 0.60 s |
| timebar | the worker selected: on cpu 0.90 s, asleep 0.80 s, of it 0.40 s in futex(2), runnable at least 0.15 s | 902 ms, 882 ms, 405 ms, 722 ms |
| wake | each relay woken by the one before it, 100 %, 180 to 300 times | 300, 299, 299; the ring and no backward edge |
| flows | the socket's bytes exact | 4.00 MiB sent, 1.00 MiB received |
| heatmap | 600 receives in a ~1.5 ms regime, 50 in a ~12 ms one, the slow phase's span 0.60 to 0.78 s | ~1.58 ms (600), ~12.1 ms (50), 539 ms to 1.22 s |
| ledger | cpu at least 0.5 s, memory grown 64 to 70 MiB, the socket's bytes exact | 640 ms, +64.3 MiB, 2.00 MiB out, 256 KiB in |
| system | two processes at 1.0 s of cpu each; the disk at least 16 MiB; loopback at least 16 MiB | 1.05 s and 1.03 s; 16.2 MiB; 36.8 MiB |
| lanes | lanes a, b, c first awake in order, awake shares within 25 % of each other; 12 kernels and a `gpu0 … spin` lane when the device served | 21.6 %, 21.4 %, 20.8 %; 12 kernels |
| request | 16 requests; each plain one under 1 ms on the cpu, each lock wait 30 to 39 ms on `lock/sleep`, each cold read on `disk`, each paused client 40 to 52 ms on `net in`, in the plan's order | all 16 as planned |

**Each view after a click** (`every_view_after_a_click` in
`tests/view_snapshots.rs`): one `<view>-click--<fixture>` snapshot per view,
the click landing on what the view drew (a process row, a frame, a ledger
line, a request, a bar, a heatmap cell, a lane, a woken thread, a file), each
read against the plain snapshot: every one moved the selection to what was
clicked. `flows` is clicked on `file-serving`, because the flows workload's
single socket is already the one selected.

**The gallery's README** (`ed52743`): retitled for the nine views kept, the
`Keep?` column "yes (2026-10-04)" on every row, the workload snapshot first in
each row, a section on the recordings and on `view_truth.rs`, the frame's
snapshots where they are now drawn, the links and the mouse.

## Shown versus reasoned

Shown: every number in the table above, by `cargo test --test view_truth` (9
passed); `cargo test --test view_snapshots` 16 of 17 (the 17th is `process`,
below); `cargo test --test cli` 27 passed (the gallery README); clippy with
`-D warnings` and `cargo fmt --check` clean.

After the review round (see "Review"): `cargo test -p omn-view` 184 passed;
`cargo test --test view_snapshots` 17 of 17; `cargo test --test view_truth`
10 passed; `cargo test --test view_model` 5 passed; `cargo test --test cli`
27 passed; clippy and fmt clean.

Shown since: that a re-recording draws the same screens within their
measurement. All nine workloads were recorded a second time (`make &&
record.sh workload-…`, 30 s) and `view_truth.rs` passed 10 of 10 on them;
the committed recordings were then put back, so the snapshots stay drawn on
the first. `workload-flows` and `workload-ledger` are committed re-recorded
(with `io.syscall`'s rows), which makes three recordings of each that pass.

Reasoned, not shown:

- That a terminal delivers the clicks the `-click--` snapshots answer: the
  harness sends the event the way crossterm reports it.

## What I could not do, and why

- The view crate's failing tests, the `process` snapshots and the bytes per
  file, listed here in the first round, are done: see "Review".
- **`--against` and focus snapshots in the standard set** (the view stream's
  request): not done for time. `snapshot::draw_with` is there; the ledger's
  two stay in `ledger.rs`.
- **The heatmap's two fast phases** are one regime on screen (`~1.58ms
  (600)`), as they should be; the test holds the pair of boundaries of the
  slow phase, not the fast phases' own.

## Privileged runs wanted

None beyond what `make` grants. To re-record (about three minutes, the GPU
in use by `workload-lanes` for under a second):

```console
make && tests/view_fixtures/record.sh                 # all twenty
make && tests/view_fixtures/record.sh workload-lanes  # one
OMN_VIEW_BLESS=1 cargo test --test view_snapshots     # then look at the diff
```

`record.sh` now refuses a binary without `cap_bpf` (a `cargo build`,
`cargo test` or `cargo clippy` since `make` relinks it and drops them): the
first try at the second recording above was made on such a binary, and
recorded every eBPF source as refused without a word. `OMN_RECORD_UNPRIVILEGED=1`
records anyway, for a machine that truly cannot.

## Decision lines

- **A view's truth is read off its screen**, not off its model: most views
  keep their model private, and the screen is what the user reads. Cost: a
  wording change in a view breaks `view_truth.rs` with a clear message
  ("no line with `…`"), which is the point.
- **The workload recordings are fixtures like the others**, so every view is
  drawn on every workload (117 new snapshot pairs, and 9 after a click). Cost:
  22 MB of fixtures in the tree; `system-wakeup` cut to one second for that.

## Needs from another stream

- **docs (`examples/vertical/server.c`)**: `MAX_PLAN` raised from 64, so a
  plan of a few hundred requests does not need the `sed` in `record.sh`.
- **coordinator**: this branch now carries the re-pinned view tests and every
  snapshot re-blessed on these recordings, `process--*` and `frame--*`
  included; merge it as one, with nothing to re-bless after it unless main
  moved a view since `17521b4`.

## Review

The reviewer's two findings, both fixed (`7b1300a` and the commit after it).

**The workspace suite red on the merge.** The 18 view unit tests now hold
properties of the recordings, not their figures: ids read from the data
(system, flame, timebar), a cause's size within the plan's bounds
(`examples/vertical-one-request.yml`) rather than one run's milliseconds
(request), the slowest call's window read off its own row (heatmap), the
histogram's floor from the fastest call (ledger). The two worth a look:

- `every_timeline_of_the_machine_ends_where_the_run_does`: the shifted track
  is the target's `cpu.oncpu_time`, not `cpu.util@system`, and the shift is
  the data's. Its result says `first_ts_ns` 16 416 138 ns after the window
  opened: the target's first read, which the view places where the run says
  it was. The view did not move it; the old recording carried no
  `first_ts_ns`, so the inference put it at the opening. The test now holds
  the track to `first_ts_ns` and to under 50 ms.
- `off_cpu_splits_by_thread_and_narrows_to_a_focused_one`: the by-thread
  build was not the one failing -- it gives its three threads. The panic is
  the narrowing: the test focused tid 2758974, a thread of the old
  recording, and the new one holds nothing of it, so the tree was `Empty`
  ("no block was recorded"). `value_attr: block_ns` is not involved. The tid
  is now read from the recording.

Reading the rest turned up three real defects, fixed rather than re-pinned:

- **A machine run claimed a target.** With `cpu.oncpu_time` on the eBPF
  backend, a run with no target keys every task on the machine; the ledger
  titled `machine` "what task 1 and 3120 more cost" with "≈ the target's"
  energy, and the frame's corner said "target 753% cpu" (and "target 520%"
  on `system-wakeup`). A result metered on the cpus (`observed_from`
  "everything but idle on …") is now the machine's account, never a
  target's (`app::metered_on_the_machine`), in the ledger and the corner.
  Held by `a_run_on_the_machine_claims_no_target` and the ledger's title
  test on both runs.
- **The system view's tests assumed a per-process cpu time**; on these
  recordings it is per thread, so the process row holds RSS and the threads
  the cpu. The view was right; the tests now say so. timebar's derived
  residue is held on the recordings put in the per-process shape it is
  about, and a new test holds the per-thread case, where the residue is
  measured and printed.
- **The view's mirror lacked `overhead.one_time_cpu_ns`**, which these
  recordings carry: `view_model`'s `the_mirror_holds_every_field_omn_writes`
  failed. Added.

`process--*` (21 fixtures) and `frame--help`/`frame--narrow` are blessed
here on these recordings, now that main carries the process view.

**Bytes per file not held.** flows' and ledger's `io.syscall` need now asks
`raw` (`docs/view/README.md`'s table with it), `workload-flows` and
`workload-ledger` are re-recorded with it, and
`flows_draws_each_files_bytes_exactly` holds `large.bin` (fd 40: 8.00 MiB
written, 8.00 MiB read) and `small.bin` (fd 41: 1.00 MiB written, nothing
read) exactly, per descriptor as the manifest counts them; the ledger test
holds its 4.00 MiB written, within 1 % above. The other recordings keep
`io.syscall` without rows until they are next recorded, and their screens say
so as before. "A re-recording draws the same screens" is shown, above.
