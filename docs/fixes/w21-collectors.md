# The twenty-first wave, stream collectors

Branch `feat/w21-collectors`, from `main` at `320c2aa` (stream data merged),
with `main` merged again at `f12a857` (stream run's review fixes).
`BRIEF.md`, "collectors -- the fields the views and the cases wait for";
`STATE.md`, "What remains", item 4 (`net.request`'s bytes and the start of
its longest sleep) and item 7 (`io.bytes`'s polled baseline,
`cpu.oncpu_time`'s `fs:proc` caveat); `docs/fixes/w21-data.md`'s requests
to this stream (`fields()`, the `dir` key, `provenance()` in the profile).
Measured on the development machine with other streams building beside it.

## What landed

| Commit | What |
|---|---|
| `f847a6d` | `feat(net)`: **`net.request`'s `rx_bytes`, `tx_bytes` and `holder_since_ns`**, `w20-net.md`'s layout: storage `S_RX_BYTES` 144, `S_TX_BYTES` 152, `S_HOLDER_SINCE` 160 (`STORAGE_BYTES` 144 -> 168); record `R_RX_BYTES` 168, `R_TX_BYTES` 176, `R_HOLDER_SINCE` 184 (`RECORD_BYTES` 168 -> 192). `holder_since_ns` is `S_OFF_SINCE` copied where `settle` replaces the longest sleep. `tx_bytes` is `write_seq` at the end less `write_seq` at the birth. `rx_bytes` is `copied_seq` at the end less `copied_seq` **at the first byte's arrival**, not at the birth (see the decision line): the arrival program writes it into a new socket-entry slot `K_RX_SEQ` (`SOCKET_BYTES` 48 -> 56) when its stamp stands, and the birth claims it by exchange. Both counts are modulo 2^32 and flagged known by two new net flags (`NET_RX_BYTES` 64, `NET_TX_BYTES` 128): an unseen start is an absent attribute, not a zero. The collector test's record is 192 bytes. **`Collector::fields()`** gives `never_ended`, `unborn`, `replaced`, `filtered_out` from the `undrawn()` the notes are written from, each present at zero. A caveat in `catalog/net.yaml` says all of it. |
| `c7adc61` | `feat(io,net)`: **one series per direction.** A request keyed on `dir` gets from `/proc/diskstats` one event per device and direction on `io.bytes` (`read`, `write`, `discard` where the column exists), `io.iops` (and `flush`) and `io.merges`, and from `/proc/net/dev` one per interface and direction (`rx`, `tx`) on `net.bytes`; each event's value attribute is that direction's share. **Without the key nothing changes** for any reader. `dir` is declared on those entries and their `fs:proc` backends; `net.bytes`' `netlink` and `bpf:fentry` and `io.iops`/`io.merges`' bpf backends get explicit keys without it. **`io.bytes`, `io.iops`, `io.merges` polled take their baseline in `start()`**, before the program is released, and pass over the first tick (taken microseconds after the release), so the program's first I/O is in the first interval. |
| `44b012f` | `docs(cpu)`: the **`cpu.oncpu_time` `fs:proc` caveat**: the process alone (its children reach `cutime` only as it waits for them, unread), and nothing between the last poll and the reap, with where the exact total is (the exit's `cpu_ns` from `wait4`). |
| `bb295f2`, `a21624f` | `feat(cpu)`: **`cpu.profile`'s `Collector::provenance()`**: `named_by` for every stack `stacks()` exports, under the same ids, and `executables` by `task:<tid>`, read from `/proc/<pid>/exe` at a task's first sample with a stack, while it lives. `perf_maps` stays empty (see "Not done"). |
| this | the handoff |

## Shown versus reasoned

Shown, on this machine:

- **The new `net.request` programs pass the verifier and measure exact
  bytes.** After `make` (capabilities granted), `omn -f
  examples/vertical-one-request.yml --format json`: the five requests carry
  `rx_bytes` 52, 52, 53, 51, 56 -- each exactly the length of the request
  line the client formats (`GET /?lock=..&reads=..&body=.. HTTP/1.1\r\nHost:
  omn\r\n\r\n`), `a004`'s split in two sends with a pause included -- and
  `tx_bytes` 59, 59, 59, 59, 131134, each exactly the worker's own `sent=`.
  `a001`'s `holder_since_ns` is 80 us after its birth with `holder_ns` 40.06
  ms of a 40.25 ms request; requests without a sleep carry none. The result
  carries `"fields": {"filtered_out":0,"never_ended":0,"replaced":0,"unborn":0}`.
  `cargo test --test vertical`: 16 passed, none skipped.
- `bpf::request::tests` (layout: the record struct against the program's
  offsets, the storage fields apart, the stack regions inside 512 bytes) and
  `collect::net::request::tests`: 26 passed, among them the new
  `a_requests_bytes_and_its_longest_sleeps_start_are_carried` (decoded from
  the 192 bytes at the kernel's offsets; absent without the flags; a 168-byte
  record refused) and the `fields()` assertion in
  `what_is_in_no_event_is_counted_as_numbers_and_said_from_them`.
- `omn io.bytes --system -k dev,dir -m poll@200ms -e summary -d 1s`: one
  series per device and direction (`dev:259+0 dir:read`, `dir:write`,
  `dir:discard`), `n=5` each; `io.iops -k dir` gives `read`, `write`,
  `discard`, `flush`; `net.bytes --iface lo -k iface,dir` gives `iface:lo
  dir:rx` and `dir:tx`. Tests: `a_request_keyed_on_direction_gets_one_series_per_direction`
  in `diskstats.rs` (read 8x512, write 16x512 from a synthetic file; `SeriesKey`
  under `--keys dir`) and in `dev.rs` (loopback, `dir:rx` then `dir:tx`).
- `a_polled_run_measures_from_its_start_and_not_from_its_first_tick`: I/O
  written between `start()` and the first tick is in the first interval
  (24x512 bytes, the whole increase since the start).
- **`cpu.profile`'s provenance**, `omn trace cpu.profile -m sample@99hz -e
  stack --keys task -d 2s --format json -o DIR -- python3 -c '...'`:
  `executables` `{"task:<tid>": "/usr/bin/python3.14"}`, `named_by` 47 entries
  for 47 stacks (282 frames `elf`, 355 unnamed), stack rows carrying
  `"series": "task:<tid>"`. The unit test
  `a_profile_of_this_process_names_its_own_frames` now also holds each
  stack's namers to its labels frame for frame and finds the test binary
  among the executables.

Reasoned, not shown:

- `rx_bytes` across a sequence-number wrap and under pipelined requests (the
  second request's first byte is unseen, so its `rx_bytes` is absent, like
  its `net_rx`).
- A torn read of `K_RX_SEQ`: the arrival writes the sequence word, then the
  valid word, non-atomically; a birth racing that write on another cpu could
  claim a stale or half-written start. Rare, bounded to one request, and the
  same race the first-byte stamp already has.
- `io.bytes`' first interval under a real launched program: `io.bytes` on
  `fs:proc` serves `--system` only, so a launch runs through a session; the
  unit test holds the arithmetic.

## What I could not do, and why

- **`cpu.oncpu_time` on `fs:proc`: a closing read while the target is a
  zombie.** Not possible from `src/collect/cpu/oncpu.rs`: the runner reaps
  the launched program in `ended()` (`Gated::finished`, `wait4`) before the
  closing round, and a collector is not called between the exit and the
  reap. Filed for run below; the caveat landed instead.
- **`perf_maps` in `cpu.profile`'s provenance**: `Unwinder` (in
  `src/stack.rs`, data's) keeps its `Symbolizer` private, so neither
  `perf_map_file()` nor `Symbolizer::executable()` is reachable from the
  profile. `executables` is read with `stack::executable_of(pid)` at the
  first sample instead, which misses an `execve` after it. Filed below.
- **`net.socket`'s `iface`** (views' request, not in this row's brief):
  not started.
- **`net.bytes` polled baseline at the start**: the brief names `io.bytes`;
  `/proc/net/dev`'s collector still takes its baseline at the first tick.
  The same two lines would move it.

## Privileged runs wanted

Shown here already with `make`; on another machine, after `make`:

```console
target/debug/omn -f examples/vertical-one-request.yml --format json 2>/dev/null \
  | jq -r '.results[] | select(.source=="net.request") | .fields,
      (.outputs[] | select(.events) | .events[]
       | [.keys[0].req.id, (.attrs[] | select(.name|test("_bytes$|holder_since")) | "\(.name)=\(.value)")] | join(" "))'
cargo test --test vertical
```

Wanted: `rx_bytes` 52 52 53 51 56, `tx_bytes` 59 59 59 59 131134, four
`fields` at zero, 16 passed.

## Decision lines for DECISIONS.md

- **A request's `rx_bytes` start at its first byte's arrival, not at its
  birth.** A server declares a request once it has parsed it, so `copied_seq`
  at the birth is past the request's own bytes and a birth-to-end count reads
  0 for most requests. The start is `copied_seq` when the first unclaimed
  byte arrives, kept in the socket's entry beside that byte's stamp and
  claimed by the birth; `tx_bytes` does start at the birth, because the
  response is written after it.
- **A byte count whose start was not seen is absent, not zero** (two net
  flags), as `net_rx` already is.
- **`dir` splits a counter only when asked.** `/proc/diskstats` and
  `/proc/net/dev` give one event per direction under `--keys ...,dir` and
  the one event per device or interface otherwise: every existing reader of
  `iface:lo` or `dev:259+0` reads what it read, and a series per direction
  is one key away. This supersedes the twentieth wave's "`net.bytes`
  direction stays an attribute" for requests keyed on `dir`.
- **A polled counter's baseline is taken before the release**, and the
  runner's immediate first tick is passed over rather than reported as a
  microsecond interval.

## Needs from another stream

- **run (`src/collect/mod.rs`, `src/collect/launch.rs`)**: the closing round
  before the reap. When the program's exit wakes the run, take the closing
  reads while it is a zombie -- `waitid(P_PID, pid, WEXITED | WNOHANG |
  WNOWAIT)` to see the exit in `Gated::finished`, the closing round, then
  the `wait4` reap. `cpu.oncpu_time` on `fs:proc` then reads the zombie's
  `/proc/<pid>/stat` (still whole: the zombie leader is in its thread list)
  and its last interval is no longer missing; its caveat's second sentence
  can then go.
- **run (`src/collect/mod.rs:2954`)**: `cargo clippy --workspace
  --all-targets -- -D warnings` is red on `main` itself:
  `clippy::large_enum_variant` on `enum Slot` (`Detached` at least 240
  bytes against 16). Boxing `Detached` (`Detached(Box<Detached>)`) is the
  usual answer. With that lint allowed, this branch is clippy-clean.
- **data (`src/stack.rs`)**: `Unwinder::symbolizer(&self) -> &Symbolizer`
  (one line), so `cpu.profile`'s `provenance()` fills `perf_maps` from
  `perf_map_file()` and reads `executables` from `Symbolizer::executable()`,
  which follows an `execve`, rather than from `executable_of(pid)` at the
  first sample.
- **views (`view/src/model.rs`, `views/request.rs`, `views/system.rs`)**:
  `net.request` events now carry `rx_bytes`, `tx_bytes` and
  `holder_since_ns`, and its result `fields` (read them in place of
  `Undrawn::of`'s sentences). The system view's disk and network rows can ask
  `--keys dev,dir` / `--keys iface,dir` for series per direction.
- **docs (`CATALOG.md`)**: `dir` among the keys of `io.bytes`, `io.iops`,
  `io.merges` and `net.bytes` (`fs:proc`); `net.request`'s row: `rx_bytes`,
  `tx_bytes`, `holder_since_ns`, `fields`.
- **coordinator (flaky tally)**: `scenario_cpu::a_counted_window_is_the_same_number_in_every_format_that_carries_it`
  failed once in the file's run (a format's count not within 2 % of the
  instructions retired, with other streams building), passed alone. No file
  it reads changed here.

## The tests

- `cargo test --lib -- collect::net::request bpf::request collect::io::diskstats
  collect::net::dev collect::cpu:: catalog::`: 152 passed.
- `cargo test --test family_cpu --test family_io --test family_net`: 17, 14,
  9 passed. `--test scenario_io`: 42 passed, 2 ignored. `--test scenario_net`:
  40 passed. `--test scenario_cpu`: 41 passed, 1 failed once (above), passed
  alone. `--test catalog_model`: 19 passed. `--test vertical` (not this
  row's, run for the verifier): 16 passed.
- `cargo clippy --workspace --all-targets -- -D warnings`: red on `main`'s
  `Slot` only; clean with `-A clippy::large_enum_variant`. `cargo fmt
  --check`: clean.
