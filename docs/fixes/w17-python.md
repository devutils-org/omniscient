# Stream python, seventeenth wave: an interpreter and a server

ROADMAP phase 12, cases 3 and 4. Everything here is in `cases/cpython-*` and
`cases/http-server-*`; nothing under `src/`, and nothing in `tests/cases.rs`:
every reference reads through the `key-value` extractor that is already
registered, so no extractor was needed.

---

## 1. What landed

Three case directories, each a `case.yml` and the helpers its injection and
references run (`docs/cases.md` describes the format).

- **`cases/cpython-interpreter/`** -- case 3 without a perf map. The
  distribution's `python3` 3.14.7 (dynamic symbols only, no frame pointers).
  - On the cpu, `one-function-dominates`: `hot.py` reads 2 000 `weight word`
    lines, scores each with a pure-Python loop of `weight` steps (`score`),
    keeps the ten best. The input (written by `setup` from a fixed seed,
    weights 3 000 to 9 000) makes `score` the time by construction. The
    program prints its own `cpu_ns` (`process_time_ns`) and `real_ns`
    (`monotonic_ns`) around the work; `cprofile_account.py` runs the same
    program on the same input under `cProfile` and prints each function's
    own time and share.
  - Off the cpu, `a-blocking-read`: `writer.py` creates a named pipe, holds
    it open, says `ready`, waits for the target in `/proc`, and writes one
    line 1.5 s later; `reader.py` opens the pipe and blocks in `read`. The
    references are the writer's own clock (`held_after_target_ns`,
    `poll_ns`) and the reader's clock around its read (`read_ns`).
  - Expected: `limit: cpu`, `on-cpu`, and the sentence never presenting an
    interpreter C function as the hottest (`never_says: ['hottest function
    `']`); off the cpu `sleep`, `blocked`, leaf `__read`.
- **`cases/cpython-perf-map/`** -- case 3 with `-X perf`: the same program,
  input and causes (the helpers are symbolic links to
  `cpython-interpreter`'s, so the two cases differ by the one flag). Needs
  `sys.activate_stack_trampoline`, or skips with that reason. Expected: the
  hottest function is `py::score:` on the cpu, and the block passes through
  `py::wait_for_input:` off it.
- **`cases/http-server-stdlib/`** -- case 4, `python3 -m http.server -p
  HTTP/1.1 --bind 127.0.0.1 --directory ROOT 0`, under `client.py`, which is
  both the load and the latency clock (no load generator is installed). The
  client finds the server in `/proc`, reads its port from the kernel's table
  of the server's sockets (it was asked for port 0), sends its requests on
  one keep-alive connection so one serving thread handles them all and is
  alive to be read, then reads the serving thread's
  `/proc/<pid>/task/<tid>/schedstat` and the process's `/proc/<pid>/stat`,
  closes, and stops the server with SIGINT, as a person at its terminal does.
  - On the cpu, `a-large-listing`: ten requests for a directory of 30 000
    entries (2.3 MB of escaped, quoted HTML each).
  - Off the cpu, `a-slow-client`: one request for a 16 MiB file, read 256 KiB
    at a time with a 256 KiB receive buffer and 20 ms between reads.
  - Off the cpu, `a-cold-file`: five requests for a 256 MiB file kept in
    `{cache}` (on a disk, not tmpfs), each after the client drops it from the
    page cache (`posix_fadvise(DONTNEED)` on its own descriptor) and checks
    with mincore(2) that no page was left. **Skips here with the numbers**
    (below): on this NVMe the disk is not the request's time.

## 2. Shown, and reasoned

**Shown** on the development machine (24 cpus, kernel 7.2.7, paranoid -1 as
the coordinator left it, `omn` with `cap_bpf,cap_perfmon` from `make`, other
agents running): `cargo test --test cases`, **4 passed** (88.8 s), every
check of every cause both ways, each failure listed under the case's
`open_findings` and printed as `FINDING`. The numbers of that run:

```
cpython-interpreter/one-function-dominates/cli: on-cpu 1123.4 ms; account cpu 1099.5 ms of real 1108.7 ms; cProfile score 99.95 %
cpython-interpreter/one-function-dominates/mcp: on-cpu 1084.3 ms; account cpu 1062.9 ms
cpython-interpreter/a-blocking-read/cli:        blocked 1495.4 ms (sleep, leaf __read); writer 1500.1 ms after it saw the target; read 1495.4 ms
cpython-interpreter/a-blocking-read/mcp:        blocked 1493.6 ms; writer 1500.1 ms; read 1493.7 ms
cpython-perf-map/one-function-dominates/cli:    on-cpu 1072.2 ms; account cpu 1050.8 ms; hottest `__tls_get_addr`, not `py::score`
cpython-perf-map/a-blocking-read/mcp:           blocked 1491.5 ms, through py::wait_for_input (ok)
http-server-stdlib/a-large-listing/cli:         blocked 4392.1 ms (main thread, `select`), on-cpu 74.2 ms; serving thread on a cpu 4340.1 ms
http-server-stdlib/a-slow-client/cli:           blocked 1296.4 ms (main thread, `select`); serving thread off the cpu 1286.1 ms, the client's sleeps 1283.7 ms
http-server-stdlib/a-cold-file:                 SKIPPED both ways: the serving thread's off-cpu share 0.147 and 0.152, under the 0.5 the cause needs
```

- **The known cause is first**: both CPython cases, both causes, both ways
  (`on-cpu` with `limit cpu`; `blocked` with `limit sleep`). Every cited row
  agrees with its reference: the on-cpu row runs 2 % over `process_time`
  over the work, covered by the interpreter's start outside the account and
  the profile's 1 % interrupt caveat; the blocked row is within 7 ms of the
  writer's clock.
- **What the Python function is, by cProfile and by the stacks**: cProfile
  gives `score` 99.95 % of the three functions' own time. The stacks the
  profile already collects carry `py::score` on **1 043 of 1 056 samples**
  under `--stack fp` (`omn cpu.profile --stack fp -- python3 -X perf
  hot.py`), and 419 of 438 under `--stack dwarf`, counting the innermost
  `py::` frame of each stack. So the name the brief asks for is in the
  data; what is missing is the reading (finding 1).
- **The block's Python frame is there too**: `sched.offcpu` under `--stack
  fp` and under `--stack dwarf` gives the same stack, `__read <-
  libpython+0x2ebff6 <- +0x3335ca <- +0x5dfc1 <- py::wait_for_input <- ...`,
  the Python function fifth from the leaf; the agent's way reads it
  (`the-block-s-frames-are-named` ok), the person's verdict prints four
  frames (finding 3).
- **The server's request is not where the triage looks**: the listing's
  serving thread spent 4 340 ms on a cpu by its own schedstat, the client
  waited 4 342 ms; the verdict decomposes the main thread, 98 % asleep in
  `select` (finding 4).
- **The tool's own cost on the server**, the client's clock over ten
  listings, one source at a time (`omn <source> --emit ... -- python3 -m
  http.server ...`, `scripts/` not used):

  | Run | Client's latency, 10 listings | Server cpu |
  |---|---|---|
  | bare | 2 124 ms | 2.14 s |
  | `sched.mutex` (`preload`) | **4 059 ms** | 4.09 s |
  | `cpu.profile` | 2 131 ms | 2.16 s |
  | `sched.offcpu` | 2 099 ms | 2.13 s |
  | `sched.futex` | 2 217 ms | 2.25 s |
  | `sched.runqlat` | 2 071 ms | 2.10 s |

  `cost.json` for the `sched.mutex` run: `measured_percent` 127 %, 5.4 M
  hooked calls, 1.65 s inside the target (finding 5).
- **The cold cache does not make an NVMe the request's time**: 256 MiB
  served five times, cold (mincore: 0 pages resident before each request)
  509 ms against warm 375 ms; the serving thread off the cpu 16 % cold and
  15 % warm (the warm part is the client reading its 1.3 GB). The cause's
  `holds` asks for half, so it skips with those numbers, as `docs/cases.md`
  says a cause the reference says was not there must.

**Reasoned, not run**:

- **At `perf_event_paranoid` 2**: as case 1, every cause either answers or
  skips both ways with the machine's reason first; nothing here needs more
  than `cpu.profile` and the `sched.*` sources the triage already asks. Not
  run: a machine-wide sysctl (privileged list below).
- **On the CI runner** (Ubuntu 24.04, Python 3.12): `-X perf`,
  `http.server -p` (3.11) and everything the helpers use exist there. The
  numbers differ (the hottest C leaf is this build's `__tls_get_addr`
  because Arch's `libpython` uses the general-dynamic TLS model: its text
  references `__tls_get_addr` 1 473 times); the checks are written against what any build shows
  (a `py::` frame, a `hottest function` in backquotes, `select.cpython`).
- **`a-cold-file` on slow storage**: it runs where the disk is slower than
  the copy (a spinning disk, a network volume, `dm-delay`); there the known
  cause is `disk`, and the person's way will fail
  `the-first-suspect-is-the-known-cause` for the same reason as the listing
  (finding 4), which the case already lists.

## 3. Findings the cases show, filed

Each is an `open_findings` line of the case that shows it, quoted by title.

### 1. "the hottest leaf is never the Python function" -- `src/triage/latency.rs:1033` (`hottest`), `docs/mcp.md`, `tests/cases.rs` (`read_mcp`)

`hottest()` ranks the task's samples by their leaf frame. Under `-X perf`
the perf map names the trampoline, which *calls* the evaluation loop, so the
leaf of every sample is one of the interpreter's C functions and the Python
function is never a leaf: the verdict says *"hottest function
`__tls_get_addr`, 100 of 1120 samples (9 %)"* while `py::score` is on 99 % of
the stacks and cProfile gives it 99.95 %. The agent's way reads the same
way, as `docs/mcp.md` tells a model (the leaf with the most own samples).
**Ask**: where a stack carries frames a perf map named, rank by the
innermost such frame and say so (*"hottest Python function `score`
(`py::score:/path/hot.py`, perf map), 1 043 of 1 056 samples"*); the same
reading in `docs/mcp.md`, and in `read_mcp`. Check:
`the-dominant-stack-ends-in-the-known-frame` (`cpython-perf-map`). **For
the coordinator** (`src/triage/`, `docs/mcp.md`) **and stream cases**
(`tests/cases.rs`).

### 2. "an interpreter's C function is named as the suspect" -- `src/triage/latency.rs:1054`

Without a perf map, no frame of any stack names the program's code; the
profile is flat over the interpreter (`__tls_get_addr` 9 to 11 %,
`_Py_Dealloc` 6 to 8 %, `PyLong_FromLong` 5 %). The verdict presents the
first of them as *the* hottest function. **Ask**: when the leaves are spread
over an interpreter's library and no frame of the program's own is named,
say the time is in the interpreter's frames, that a perf map would name the
functions (`python3 -X perf`, `node --perf-basic-prof`: the symbols
stream's note already has the list), and not present a C function as the
suspect. Check: `the-sentence-claims-nothing-false` (`cpython-interpreter`,
`never_says: ['hottest function `']`). **For the coordinator.**

### 3. "the verdict cuts a block's stack at four frames" -- `src/triage/latency.rs:1082`

`blocking_stack` prints `frames.iter().rev().take(4)`. Under an interpreter
the four nearest frames are the interpreter's (`__read <- libpython+... x3`)
and the Python function is the fifth; under `http.server` the frame that
tells the accept loop from a send (`select.cpython-314...so` against
`_socket.cpython-314...so`) is the second, so that one is kept. **Ask**:
print up to the innermost frame of the program's own (a perf-map frame, or
the executable's), or more frames. **Not an `open_findings` line**: the
person's way's check prints `unchecked` here (request 1 to stream cases),
and a listed check that is `ok` elsewhere and never `FAILED` would fail the
test. **For the coordinator.**

### 4. "a server's request is not its main thread's life" -- `src/triage/latency.rs:820` (`main_task`), the module doc at `:10`

The triage decomposes the launched program's main thread (*"A program's
latency is its main thread's life"*). `http.server` -- like most servers --
waits for connections on its main thread and serves each on a thread of its
own, so the verdict answers how the accept loop spent its life: `sleep`,
98 %, *"leaf first: libc.so.6+0xa08f2 <- select.cpython-314-x86_64-linux-gnu.so+0x3fd3
<- ..."*, for a listing whose serving thread was on a cpu 4 340 of the
client's 4 342 ms. The slow client's `blocked` passes
`the-first-suspect-is-the-known-cause` by accident: the accept loop sleeps
too, and only `never_says: [select.cpython]` shows it. **Ask**, in order of
reach: (a) when the main thread's largest row is a block in which other
threads of the program ran, say so and name the busiest other thread's
decomposition beside it -- the JSON already lists them; (b) an option to
decompose a named thread, or the threads a request crossed
(`omn latency --thread busiest`). Checks: `the-first-suspect-is-the-known-cause`,
`every-cited-number-agrees-with-the-reference`,
`the-sentence-claims-nothing-false` (`http-server-stdlib`). **For the
coordinator.**

### 5. "the triage's mutex source doubles a threaded interpreter's time" -- `src/triage/latency.rs:181` (`sched.mutex` in every session)

`omn latency` asks `sched.mutex` of every target, and it resolves to
`preload` (declared `extreme`). CPython takes a pthread mutex millions of
times a second once a second thread exists: on the listing it hooked 5.4 M
calls, spent 1.65 s inside the target, and **the client's latency went from
2 124 ms to 4 059 ms** -- the triage doubles the time it is asked to
explain. Each of the other four sources, alone, costs 0 to 4 %. `cost.json` says
it (`measured_percent` 127); the verdict does not. And through
`run_session` the required budget stops the run: *"[sched.mutex]
--budget 100% was exceeded: 132.94 %"*. **Ask**: the triage asks
`sched.mutex` only through a backend under its own cost ceiling (`bpf:usdt`
or `bpf:uprobe` when `preload` is the only one left, or none), or asks it
after the cheap sources show a sleeping row to split; and the verdict says
what each source cost when one cost more than the criterion. Check:
`the-agent-s-way-answers-within-a-model-s-budget` (`http-server-stdlib`).
**For the coordinator.**

### 6. "dynamic symbols only is not said" -- `src/stack.rs:2503` (`facts`)

The `names came from` line is written only when some module was named by
debuginfod or a perf map or not at all. A run whose every module carries its
dynamic symbols only -- `python3`, `libpython3.14.so`, `libc.so.6`, the
interpreter's internal functions unnamed and printed as `libpython+0x...` --
says nothing about where its names came from, so the agent's way cannot say
how it knows. With `-X perf` the same modules are described (the perf map
triggers the line). **Ask**: `NameOrigin::DynamicOnly` (or whatever the
`its dynamic symbols only` origin is called) among those that trigger the
line. Check: `the-verdict-says-how-it-knows` (agent's way;
`cpython-interpreter`, `http-server-stdlib`), and the person's way's
`the-person-s-way-says-where-names-came-from`, which also needs case 1's
"the verdict drops where its names came from". **For stream symbols / the
coordinator** (`src/stack.rs`).

### Also shown, already filed

- `the-agent-s-way-answers-within-a-model-s-budget` on both `a-blocking-read`
  causes: the reader burns 14 ms in 1.5 s and `sched.offcpu` or
  `sched.futex` alone spend more at `--budget 100%`: case 1's "a waiting
  program exceeds every budget".
- `the-person-s-way-says-where-names-came-from` everywhere: case 1's "the
  verdict drops where its names came from".

## 4. What I could not do, and why

- **Run the cold-file cause to an answer**: this machine's NVMe serves the
  file faster than `http.server` copies it. Throttling it needs root (`io`
  is not delegated to the user's systemd: `cgroup.controllers` holds `cpu
  memory pids`) or `dm-delay` over a loop device: privileged, listed below.
- **Change the triage, `docs/mcp.md`, `src/stack.rs` or `tests/cases.rs`**:
  not this stream's files; filed above and below.
- **A positive check for "interpreter frames"**: the format has
  `never_says` and no `says`, so `cpython-interpreter` asserts only that no
  C function is presented as the hottest (request 3).
- **`tail`**: no case asks it (`--runs` is not passed by the harness yet).

## 5. Tests

- While iterating: `cargo test --test cases` only (each run 50 to 90 s, all
  four cases).
- Before the last commit, one `make check`: **red** at
  `tests/scenario_io.rs::driving_the_block_tracer_past_the_ring_says_how_much_it_lost`
  (*"1096 records were dropped and the run does not say the ring
  overflowed"*), a scenario in `docs/scenarios/io.md` (section 12), outside
  this stream's files; `cases` was green inside it (4 passed, 84.4 s). Then
  one `cargo test --workspace --no-fail-fast`: **2187 passed, 2 failed, 10
  ignored**.
  - `scenario_gpu::the_process_figure_and_the_device_figure_move_by_the_same_step`
    (*"the process gained 268435456 bytes and the device 0"*): rerun alone
    once, passed. Not this stream's.
  - `cases::every_case_names_its_known_cause_both_ways`, on
    `cpython-perf-map/a-blocking-read/mcp`: `every-cited-number-agrees-with-the-reference`,
    *"the reference `account`: no `key=value` number in"* `run_session`'s
    `said_by_omn` -- the reader's own `read_ns` line was not in what the
    server handed back, which held omn's notes, costs and caveats only
    (window 929.5 ms, opened 1.06 s after the start, the whole suite and
    other agents loading the machine). Rerun alone once (`cargo test --test
    cases`): every check of the three cases green; that rerun failed
    instead on case 1's `a-lock-held-elsewhere/cli` (*"the run names no
    single state: ... 1.900s of the wall clock is in no row"*), load again,
    not this stream's. Not reproduced, so not filed as a finding; **for
    stream cases**: if it recurs, the program's stderr line missing from
    `said_by_omn` (or the run ending before the program printed it without
    an `exceeded` line the harness recognises) is the place to look.
  - The io failure of `make check` passed in the no-fail-fast run, and
    alone once.
  - The client's clean exit when a budget stops the server (below) landed
    after this `make check` started; the cases rerun above ran with it.

## 6. Privileged runs wanted (the coordinator, alone)

```console
cd /home/moha/repos/omniscient-w17-python && make
# The CI's defaults cell: every cause answers, or skips both ways with the machine's reason first
sudo -n /usr/bin/sysctl kernel.perf_event_paranoid=2
cargo test --test cases -- --nocapture 2>&1 | grep -E '^case |^  |SKIPPED|test result'
sudo -n /usr/bin/sysctl kernel.perf_event_paranoid=-1 && make

# The cold-file cause on slow storage: a 256 MiB file on dm-delay (50 ms reads)
# over a loop device named omn-*, served by the same server and client.
truncate -s 400M /var/tmp/omn-cold.img && LOOP=$(sudo -n losetup --show -f /var/tmp/omn-cold.img)
echo "0 $(sudo -n blockdev --getsz $LOOP) delay $LOOP 0 50" | sudo -n dmsetup create omn-cold
sudo -n mkfs.ext4 -q /dev/mapper/omn-cold && sudo -n mount /dev/mapper/omn-cold /mnt && sudo -n chown $USER /mnt
mkdir -p /mnt/www && head -c $((256<<20)) /dev/urandom > /mnt/www/cold.bin
cd cases/http-server-stdlib && (python3 client.py /mnt/www /cold.bin 2 --cold /mnt/www/cold.bin &) ; sleep 0.2
../../target/debug/omn latency --objective latency -- python3 -m http.server -p HTTP/1.1 --bind 127.0.0.1 --directory /mnt/www 0
sudo -n umount /mnt && sudo -n dmsetup remove omn-cold && sudo -n losetup -d $LOOP && rm /var/tmp/omn-cold.img
```

The second is a functional look, not a test: what the person's way says when
the disk is the request's time (expected today: the main thread's `select`,
finding 4). `mkfs.ext4`, `mount` and `chown` may not be in the sudoers file;
if not, `docs/scenarios/io.md` section 18's dm-delay recipe (a loop device
written through directly) is the one to adapt.

## 7. Decision lines for `DECISIONS.md`

| Question | Decision | Why |
|---|---|---|
| How a case asks an interpreter with and without its perf map | **Two cases differing by the one flag** (`cpython-interpreter`, `cpython-perf-map`), the helpers shared by symbolic links. | A case's `needs` and `open_findings` are per case: the trampoline is needed by one only, and the two show different findings. |
| The cProfile reference | **A second run of the same program on the same input, used for *which* function, never *how long*.** | cProfile triples the run's time; its shares are robust, its durations are not the measured run's. The duration is the program's own `process_time`. |
| A server as a case's target | **Started on port 0, found and stopped by the case's client** (port from `/proc/<pid>/net/tcp`, SIGINT as at a terminal); requests on one keep-alive connection. | The tool launches the program as a person would; the client is the load and the clock; one serving thread alive at the end is what lets its `schedstat` be read. |
| A cause this machine's hardware cannot make dominant | **Kept, with `holds` saying when it is there; it skips with the numbers elsewhere.** | The cold cache is a real cause on most storage; a NVMe hides it, and asserting it here would assert the disk. |

## 8. Needs from another stream

- **stream cases** (`tests/cases.rs`, `docs/cases.md`):
  1. `the-block-s-frames-are-named` is `unchecked` (*"the case asks for the
     file's own names only"*) whenever a frame is an address and the case
     says `symbols: local`. Under `local` the file's own names are all there
     will be, so an absent frame is absent, not unnamed: `unchecked` only
     where names *could* have been had (debuginfod asked and unavailable).
     This hides finding 3 in `cpython-perf-map`.
  2. `read_mcp` (and the person's reading) take the hottest *leaf*: rank by
     the innermost frame a perf map named when there is one (finding 1), and
     give a cpu answer the frames of its hottest stack, not the leaf alone,
     so `through` means the same on and off the cpu.
  3. A positive expectation, `says: [...]`, beside `never_says`, so
     `cpython-interpreter` can require "interpreter" once the triage says it;
     and `never_says` read against the agent's way's dominant stack too
     (today the agent's slow-client answer passes on the accept loop's block
     with nothing to catch it).
  4. `docs/cases.md`, "The cases" table, three rows:
     `cpython-interpreter` (python3 3.14, no perf map | `score`, made
     dominant by the input | a read of a named pipe written 1.5 s after the
     target appears | cProfile, the program's clocks; the writer's clock);
     `cpython-perf-map` (the same under `-X perf`; names from the perf map);
     `http-server-stdlib` (`python3 -m http.server` | a 30 000-entry
     listing | a slow client; a cold 256 MiB file, skipped where storage is
     fast | the client's clock, the serving thread's schedstat, the
     process's stat).
  5. A note for the flaky tally: `both-ways-name-the-same-suspect` on
     `cpython-interpreter/one-function-dominates` compares two named C
     leaves of a flat profile (`__tls_get_addr` about 10 %, `_Py_Dealloc`
     about 7 %); in every run so far both ways named the first, but a run
     that flips them fails a check that is not listed. Finding 2's fix
     removes the comparison of C leaves altogether.
- **coordinator** (`src/triage/latency.rs`, `docs/mcp.md`): findings 1 to 5.
- **stream symbols, or the coordinator** (`src/stack.rs`): finding 6.
- Also changed after the first commit: `client.py` exits with one line
  (*"the server stopped answering GET ..."*) instead of a traceback when a
  budget stops the server under a request.
- **coordinator** (`.github/workflows/ci.yml`): nothing to install for these
  cases (python3 is on the runner); `cargo test --test cases` already runs
  them once it runs case 1.
