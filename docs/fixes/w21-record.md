# The twenty-first wave, stream record

The recordings, last: every fixture re-recorded on the merged tree
(`process`'s with its new session), every snapshot re-blessed, `process` in
the gallery with "yes (2026-10-04)", and `tests/view_truth.rs` and
`tests/view_workloads.rs` holding each view to its manifest. Branch
`feat/w21-record`, on `main` at `ed9140c` (every other stream merged).

## What landed

| Commit | What |
|---|---|
| `408239e` | **Every fixture re-recorded** (`make && tests/view_fixtures/record.sh`, 2 min 16 s, on a binary carrying `cap_bpf,cap_perfmon`): the 20 fixtures of `tests/view_fixtures/` and, new in `record.sh`, **`process-run`** (`record.sh process-run`), the process workload launched as its manifest's `run` says, under every view's needs, into `docs/view/gallery/process-run/` (`io.latency` among its 22 sources, as the process stream asked). `record.sh` no longer `sed`s `MAX_PLAN` (the docs stream's request: `server.c` holds 1024). `cpython-hot`, `cpython-hot-heavier` and `process-run` were recorded twice; see "Shown". 21 MB of fixtures (22 MB before), `process-run` 680 KB. |
| `21577fd` | **Four view tests held to properties the recordings keep** (files outside this row, see "Edits outside this row"). |
| `cf574fe` | **`tests/view_truth.rs`**: `system`'s disk and loopback read every figure on a device's or an interface's line (since `KeyKind::Dir`, loopback prints `8.02MiB in  8.02MiB out`, and a disk `0B read  16.2MiB written`; the test read the first figure only). New: **`process_draws_each_line_as_its_workload_loaded_it`**, on `process-run`: CPU's peak at least 0.8 of the one core `crunch` burns, the resident peak at least the 32 MiB touched, `↓`/`↑` times the program's life within 2 % of the socket's bytes, `lock` a state of its own on the Time line. |
| `07b47ae` | **Every snapshot re-blessed** (`OMN_VIEW_BLESS=1` on `cargo test -p omn-view` and `--test view_snapshots`): 477 files under `docs/view/gallery/`. **The gallery's README**: ten views kept; `process`'s row rewritten for the system view it now is, "yes (2026-10-04)"; the flows row's `sqlite-commits` now draws the shell's descriptors; the trust style keeps the usage hue on `system` and `process`; `process-run/` named as `record.sh`'s. |

## Shown versus reasoned

Shown, on the final recordings:

- `cargo test --test view_truth`: 11 passed (the new process test included).
- `cargo test --test view_snapshots`: 17 passed.
- `cargo test -p omn-view`: 195 passed, **1 failed**:
  `views::process::tests::the_lines_find_what_the_workload_did` -- see
  "Not done".
- `cargo test --test view_workloads` (live, with the capabilities): 14
  passed, after the ledger manifest excused `sched.offcpu`
  (`each_session_asks_its_views_sources_or_says_why_not` failed before it).
- `cargo test --test cli`: 28 passed.
- `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt
  --check`: clean.
- **Two flukes of a recording, shown by recording again**: the first
  `cpython-hot` caught five cold read-ahead block requests (4.7 ms off cpu,
  the interpreter's library out of the page cache), which broke three flame
  tests and the heatmap's `the_program_s_own_latency_is_drawn_before_the_scheduler_s`;
  the first `cpython-hot-heavier` spent less than `cpython-hot` in
  `PyObject_Size` (`twice_the_work_moves_everything_absolutely_and_little_in_shape`).
  Both re-recorded once, and all of those pass without a change to a test.
- **The Disk shortfall is not a fluke**: `process-run` recorded twice gave
  the same 4136960 bytes of 4194304. A machine-wide `io.latency` trace (one
  more run of the workload, `target: system`, raw) showed why: 31 of the
  file's 32 128 KiB `WS` requests carry `process-main`, the 32nd
  `kworker/3:1H` (the block layer's kblockd work item dispatching it), with
  no loss counted.

Reasoned, not shown: that `system-wakeup` at 246 KB against 2.2 MB before is
a quieter machine (fewer wakeups in its one second), not a lost source; its
losses are zero and its snapshots draw.

## Not done, and why

- **`views::process::tests::the_lines_find_what_the_workload_did` stays
  red.** It holds the Disk line to at least the file's 4 MiB; the line shows
  the process's own block requests, and one of 32 is charged to a kernel
  worker (above). The test states a property the collector does not give;
  which side moves is for collectors and process (requests below). Not
  papered over here.
- **`--against` and focus snapshots in the standard set** (the views
  stream's request, w20 and w21): not done for time.

## Privileged runs wanted

None beyond `make`. To re-record:

```console
make && tests/view_fixtures/record.sh                 # all, process-run included, about 2.5 min
make && tests/view_fixtures/record.sh process-run     # one
OMN_VIEW_BLESS=1 cargo test -p omn-view && OMN_VIEW_BLESS=1 cargo test --test view_snapshots
```

To see the kworker charge again (unprivileged beyond `make`'s capabilities):

```console
cat > /tmp/io.yml <<'EOF'
name: io check
duration: 8s
requests:
- {source: io.latency, label: sys, mode: trace, emit: [raw], target: system}
EOF
target/debug/omn top --headless -f /tmp/io.yml -o /tmp/iosys &
sleep 1.5; python3 -X perf view/workloads/process/workload.py; wait
# then count the `W*` rows of /tmp/iosys/results.json by comm: one 131072-byte WS is a kworker's
```

## Decision lines

- **A recording that breaks a view test is recorded once more before the
  test moves.** Two of the three cases here were the machine's (a cold page
  cache, sampling noise) and passed on the second recording; the third
  repeated exactly and is a finding.
- **A view test that assumed an old recording's shape builds the shape it
  needs** (`without_rows`, the fields cleared, the cursor on a request's
  block request) rather than depending on which fixture still lacks a
  source.
- **The ledger workload excuses `sched.offcpu`** rather than asking it: it
  sets no blocked time it knows, and the timebar workload's is the one held
  to a manifest.

## Edits outside this row (for the coordinator's sign-off)

All test-only, all caused by the re-recording, in `21577fd`:

- `view/src/views/heatmap.rs`, `enter_on_a_request_s_measurement_opens_the_request`:
  the cursor is put on the slowest block request made for a request; on the
  new `vertical-requests` the run's slowest is the server's setup write
  (4.76 ms, no request), so Enter opened `lanes`, as it should.
- `view/src/views/request.rs`, `abandoned()`: clears `net.request`'s
  published `fields`; the recording now publishes zeros (the collectors
  stream's `Collector::fields()`), which win over the sentences the helper
  adds.
- `view/src/views/flows.rs`: `without_rows()` strips `io.syscall`'s and
  `net.socket`'s `raw` for the two tests of a run without rows; every
  recording now asks for them.
- `view/workloads/ledger/manifest.json` (no row owns it): `not_exercised`
  gains `sched.offcpu`, which the ledger now needs for its blocked line.

## Needs from another stream

- **collectors (`src/collect/io/**`, `io.latency`)**: a block request
  dispatched from kblockd's work item is charged to the `kworker`, not to
  the task that submitted the bio, so a target's Disk bytes fall short by
  whole requests with no loss said. Charge the request to the bio's
  submitter (the `block_bio_queue`/`block_getrq` task) or count the
  requests handed to a worker as a loss of attribution.
- **process (`view/src/views/process.rs`)**: until then,
  `the_lines_find_what_the_workload_did`'s Disk bound either names the
  worker's share as a known shortfall or reads the run's machine-wide
  device bytes; it fails on both recordings made here.
- **views**: `--against` and focus snapshots through `snapshot::draw_with`,
  still open.
- **coordinator**: merge this branch last, as one; nothing to re-bless after
  it unless a view moved since `ed9140c`.
