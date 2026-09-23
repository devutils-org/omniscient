# The vertical case — one request, boundary by boundary

**Where did one request's latency go?** `ROADMAP.md` phase 4 calls this the
real milestone: a request that reads a file and sends it back, decomposed
across the boundaries it crosses, every line carrying its evidence, checked
against a server whose delays the test injected. This document is stream V's
deliverable and its handoff.

| Session | What it shows |
|---|---|
| `examples/vertical-one-request.yml` | three requests to a real server with injected delays -- a lock, cold reads, both -- each decomposed by the tool, per request |
| `examples/vertical-abandoned.yml` | a request the server declares and never ends: counted, named, and kept out of the next one |

The checks are in `tests/vertical.rs`, ten scenarios. Every number below was
produced on the development machine: an AMD Zen 2 with 24 cpus, an NVMe disk
with the `none` scheduler, btrfs, kernel 7.2, with other streams' suites
running beside it.

---

## What the tool says

`omn -f examples/vertical-one-request.yml`, the two tables, trimmed:

```
decompose one-request  per req, in ns

  req:a001
    STATE        SOURCE             VALUE        KIND     EVIDENCE          AMOUNT   RESOLUTION   SHARE
    cpu_on       net.request        oncpu_ns     traced   KEY              45.98us        exact   0.1 %
    sched_wait   net.request        runq_ns      traced   KEY              21.48us        exact   0.1 %
    lock_wait    net.request        sleep_ns     traced   KEY              40.06ms        exact  99.8 %
    block_io     net.request        disk_ns      traced   KEY                  0ns        exact   0.0 %
    whole        net.request        total_ns     traced   KEY              40.12ms        exact
    RESIDUE      whole - parts                                                +0ns        +-0ns

  req:a002
    cpu_on       net.request        oncpu_ns     traced   KEY             672.33us        exact   6.8 %
    sched_wait   net.request        runq_ns      traced   KEY             203.05us        exact   2.1 %
    lock_wait    net.request        sleep_ns     traced   KEY                  0ns        exact   0.0 %
    block_io     net.request        disk_ns      traced   KEY               8.99ms        exact  91.1 %
    whole        net.request        total_ns     traced   KEY               9.86ms        exact
    RESIDUE      whole - parts                                                +0ns        +-0ns

decompose block-io  per req, in ns

  req:a002
    device       io.latency         latency_ns   traced   KEY               8.33ms        exact  92.7 %
    whole        net.request        disk_ns      traced   KEY               8.99ms        exact
    RESIDUE      whole - parts                                           +651.68us        +-0ns
```

Against what the server and its client measured on their own clocks, in one
run taken while the full suite was loading the machine (the tables above are
another, quiet, run):

| | injected | the tool | the reference | client latency | rows' sum |
|---|---|---|---|---|---|
| `req:a001` | 40 ms under a lock | `lock_wait` 40.064 ms, holder named; no block request | holder's `nanosleep` 40.061 ms; worker's lock clock 40.095 ms | 40.215 ms | 40.153 ms (+0.15 %) |
| `req:a002` | 64 cold pages | `block_io` 8.630 ms, of which the device 8.371 ms over **64** requests carrying `req:a002`; `lock_wait` 0 | 64 reads, 11.617 ms on the worker's clock | 11.842 ms | 11.792 ms (+0.42 %) |
| `req:a003` | 20 ms, then 32 pages | `lock_wait` 20.059 ms; `block_io` 1.986 ms, the device 1.857 ms over **32** requests | holder 20.059 ms; 32 reads, 3.981 ms | 24.170 ms | 24.129 ms (+0.17 %) |

Five consecutive quiet runs put the gap between the client's latency and the
rows' sum at **+0.11 % to +0.39 %**, and the loaded one at +0.42 %, against the
roadmap's 10 %. The gap is positive
every time, as it has to be: it is the loopback in both directions, the accept,
the parse before the birth and the client's own wakeup, none of which is the
server's.

The two tables are each read per request, by the id the request carried:

- **`one-request`** is where the request's life went. Its whole is the life,
  from the server's `request__start` to its `request__done`, and the four rows
  are the four states the serving thread was in. They tile the life exactly,
  so the residue is zero by construction and says nothing -- what says
  something is which row each injected delay landed in.
- **`block-io`** opens the last row. Its whole is the request's uninterruptible
  wait, and its one part is the device's time for every block request that
  carries the request's id. The residue is what the device did not do: the
  submission, and the completion's trip back to the reader.

---

## Where a request is born: the server says so

The brief left this open and called it the important decision: derive a
request from the connection -- the socket cookie and a sequence number, at the
first receive -- or let the server declare it with a USDT probe.

**The server declares it.** Two probes, `request__start(id)` where a request
begins and `request__done(id)` where it ends, any provider, the id as the
first argument. The birth writes the id into the serving thread's own task
storage; everything else follows from that.

Connection-derived birth was rejected because it is a guess about exactly the
thing the mechanism exists to make exact:

- **It cannot say where a request ends.** A request is over when the server
  says it is, which is not its first send (a response can take several), not
  the close (keep-alive), and not the next receive (which is only known after
  the fact -- by then the thread has spent an unknown time idle, and that time
  would be credited to the request).
- **It merges what the program keeps apart.** A pipelined keep-alive connection
  carries two requests in one receive; a connection-derived id calls them one.
- **It is lost at a hand-off.** A server that reads a request in one thread and
  serves it in a pool loses a connection-derived identity at the queue, and
  the reads the pool thread issues carry nothing -- the common architecture is
  the one it cannot see.
- **Its evidence would be overstated.** The whole point of a propagated key is
  that the downstream event carries what the upstream one put there. A key the
  tool synthesised from a socket and a counter is an inference presented as an
  identity, and this project's evidence ladder exists to refuse exactly that.

What USDT costs is the program's cooperation, and it is cheap cooperation: two
`nop`s at two places the program already knows, and an id the program chose --
the one its own logs carry, so a line of the tool's output and a line of the
server's log name the same request. What it cannot reach is a server that
declares nothing, and for that the run says so rather than guessing:

```
$ omn net.request --emit raw -- /bin/true
error: cannot start collecting `net.request`: /bin/true declares no request:
`net.request` follows a request from a `request__start(id)` probe to a
`request__done(id)` probe the program fires itself, any provider, and this one
carries no USDT probe at all. Nothing is inferred from the connection instead
-- see `docs/scenarios/vertical.md`
```

A connection-derived backend can be added later as a *second* backend of
`net.request`, with a `degraded:` that says what it guesses. It is not
equivalent, and it should never be the first.

---

## The mechanism

`src/bpf/request.rs` has the programs and the long version.

**One task-storage map per process**, `BPF_MAP_TYPE_TASK_STORAGE`, created the
first time either side asks for it. The kernel insists on a BTF description of
its key and value, so `src/bpf/map.rs` builds the smallest BTF that satisfies
`bpf_local_storage_map_alloc_check` -- an `int` key and an array of `int` for a
value -- and `src/bpf/syscall.rs` gained `BPF_BTF_LOAD` to hand it over. The
map lives in the task: an entry dies with its thread, so a recycled tid never
inherits a stale id, which a hash keyed by tid would allow.

**The birth** is a uprobe-multi program on each `request__start` site. It reads
the id -- at `-O0` the server passes it as `8@-680(%rbp)`, a memory operand,
read with `bpf_probe_read_user` -- creates the thread's storage entry, and
starts the clock.

**The block layer reads it at the issue.** `src/bpf/io.rs`'s open program at
`block_rq_issue` runs in the submitting thread, calls
`bpf_get_current_task_btf` and `bpf_task_storage_get`, and copies the id into
the in-flight entry; the completion copies it onto the record. It is read
nowhere else: at `block_rq_complete` the current task is whatever the
interrupt landed on, and reading *its* storage would credit the device's time
to a request that thread happened to be serving -- a propagated key, and the
wrong one. `the_request_is_read_where_the_submitter_is_current_and_nowhere_else`
in `io.rs` holds that in the instruction stream. A kernel that cannot make the
map still gets `io.latency`, without `req`, and the run says so.

**The states.** Between birth and end, `sched_switch` moves the thread between
on a cpu, runnable, asleep interruptibly and asleep uninterruptibly, and each
stretch is added to the entry. The one subtlety is that at `sched_switch` the
program runs in the thread *leaving* the cpu, and the one arriving is a raw
pointer whose storage a raw tracepoint cannot reach. So each cpu remembers when
it last switched -- which is always the switch-in of whoever is on it now --
and a thread settles its own off-cpu stretch the next time it is current. An
off-cpu stretch whose switch-in cannot be placed (a switch the program did not
see) is counted as an `anomaly` and its time goes to no state; none occurred in
any run here.

**The end** settles the last stretch, publishes the request, and deletes the
id from the thread -- before publishing, so nothing the thread does after its
end is credited to it even when the ring is full.

---

## The ground truth, and what each scenario leans on

The four states add up to the total by construction, and no scenario treats
that as evidence. The server and its client print their own clocks on stderr,
prefixed `vertical:`, and the tool reads none of it.

| Scenario | Reference | Bound |
|---|---|---|
| `each_injected_delay_lands_in_the_row_it_was_injected_into` | the holder's clock around its `nanosleep` under the mutex; the worker's around the lock and the reads | `lock_wait` in [held − 1 ms, worker's lock clock]; no lock, under 1 ms; device ≤ the worker's read clock and ≤ `block_io` + `cpu_on`; no reads, no block request |
| `every_block_request_a_request_submitted_carries_its_id_and_no_other_does` | the number of reads, one page each with `O_DIRECT`; the kernel's own `start_time_ns` on each block request | exactly *n* requests at *n* sectors, each created inside the request's life, **no slack**; the file's creation, by the same thread before any request, carries no id |
| `the_segments_sum_to_the_latency_the_client_measured` | the client's clock, send to last byte | the rows within 10 % |
| `the_lock_wait_names_the_thread_that_held_the_lock` | the tid the holder prints | `holder_tid` equal |
| `only_the_thread_that_declared_a_request_is_followed` | the worker's tid | every request on it; the client and the holder, sleeping beside it in the same process, carry none |
| `the_tool_decomposes_each_request_against_its_own_life` | the server's clock around the two probes | the whole is the request's total, the total within that clock, the device row the tagged requests' sum |
| `a_request_that_never_ends_is_counted_and_named_and_does_not_leak` | a request the server abandons | "2 born, 1 ended", the abandoned one named with its thread, the second birth reported as a restart, and the next request whole |
| `a_program_that_declares_no_request_is_refused_by_name`, `a_mode_that_cannot_follow_a_request_is_refused_by_name` | -- | refused, naming the two probes and the mode |
| `without_the_capabilities_the_session_says_which_it_needs` | a copy of the binary, which carries no file capabilities | refused, naming `CAP_BPF` |

**What the roadmap asked for and this machine cannot give**: `tc netem` and
`dm-delay` need root. The network delay would have gone into `net_rx`/`net_tx`
and the device delay into `block_io` at the device end; the lock and the cold
read are the two injections this machine can make without root, and they are
the two rows the tests hold. No scenario pretends to have the other two.

---

## What the measurement taught

**1. `sched_wakeup` names the idle task as every lock's holder.** The first run
recorded `holder_tid = 0` on both lock-bearing requests. The kernel queues a
wakeup onto the wakee's cpu, and `sched_wakeup` fires where the task is
enqueued -- in whatever that cpu was running, here the idle loop. The waker's
identity is only in `sched_waking`, at the top of `try_to_wake_up`, in the
waker's own context. With that one change the holder is named on every run.
This is the same hop the I/O crossing found from the other side (boundary 8 is
not a wakeup from the interrupt), and anything else in the tree that credits a
wakeup to `current` at `sched_wakeup` has the same problem.

**2. A device-time row cannot cover a request's wait for its I/O.** The first
version put `io.latency`'s device time in the life table as `block_io`. On a
quiet machine the rows then summed to the client's latency within 3 %; on the
third run, with the machine loaded, `req:a002` came in at **+17 %**: 9.84 ms of
rows against an 11.83 ms whole. The missing 2 ms were real and were the
request's: on btrfs a direct read's completion is delivered by a kworker that
verifies checksums before waking the reader, and under load that kworker waits
for a cpu. The device had finished; the request had not. So the life table
reads the request's own uninterruptible wait, and the device's time is the
second table's part, with the hop as its residue -- 0.5 to 0.7 ms per request
here on a quiet machine, and signed, never rounded away.

**3. The first `O_DIRECT` page of a cold file on btrfs is a readahead.** 64
reads gave 63 `RS` block requests of 4 KiB and one `RA` of 16 KiB. Machine-wide,
the 16 KiB one sits at exactly one stride before the other 63: it *is* the
read of the file's first page, issued as a 16 KiB readahead by the thread that
asked, carrying the request's id. One request per read still holds, and the
scenario asserts it one for one; what it cannot assert is that every request is
4 KiB.

**4. The engine tables a decomposition by the keys its parts carry.**
`req:a001` read nothing, so no `io.latency` event carries its id and it has no
`block-io` table. For that request it is right. For a request that waited
uninterruptibly on something no block request of its own explains -- a page
lock, a metadata read in a kworker -- it would be wrong: it would be absent
rather than shown with its whole wait in the residue, which is the one table
that would have said something. Filed for `src/join.rs`; the scenario asserts
today's behaviour so the day it changes is seen.

**5. Every row says KEY.** The decomposition engine writes `Evidence::Key` on
every row, whatever it is read by. A row read per `req` is read by an identity
the upstream event propagated, and the join engine already rates a join on
`req` IDENTITY; the decomposition does not yet. The tables above are therefore
under-rated, never over-rated. Filed for `src/join.rs`.

---

## The roadmap's ten boundaries, and where this build is

| # | Boundary | `ROADMAP.md` | This build | Evidence |
|---|---|---|---|---|
| 1 | NIC → softirq → TCP | `sock:cookie` | not in the table: on loopback there is no NIC, the receive runs in the client's send, before the request is born | -- |
| 2 | socket ready → thread woken | WAKEUP | not in the table: the thread waits for the request before it exists | -- |
| 3 | request entry | `bpf_task_storage`, IDENTITY | **`request__start`, the id in the thread's storage** | IDENTITY |
| 4 | lock wait | WAKEUP → holder | **`lock_wait`, split from the runnable wait at `sched_waking`, `holder_tid` named** | the row says KEY (finding 5); the holder is WAKEUP |
| 5 | `read()` → cache miss | `file:dev+ino` | not joined: the id makes it unnecessary for the block row | -- |
| 6 | FS → block submission | re-read task storage at `block_rq_issue`, IDENTITY | **`io.latency` carries `req:<id>`, read at the issue** | IDENTITY |
| 7 | submit → completion | `blk:rq@ts` | `io.latency` on `blk:rq@open_ts`, unchanged | KEY |
| 8 | completion → wakeup | WAKEUP | the `block-io` residue: the kworker's hop, measured, not joined | -- |
| 9 | response | `req:id` → `sock:cookie` | `request__done`: the response is on-cpu time inside the request | -- |
| 10 | TCP TX → NIC | `SO_TIMESTAMPING` | not built | -- |

The rows the roadmap's tree drew as `net_rx` and `net_tx` are the two this
build does not have. On loopback they are the difference between the client's
latency and the request's life -- 24 to 60 µs across five runs here -- and a row for them needs
the connection's cookie at the request's birth and end: `bpf_get_socket_cookie`
on the socket the server received on, which the birth probe would have to be
given as an argument or read from the thread's last receive. That is the next
step, and it is a second argument on the same two probes.

---

## Limits, stated

- **Probes are looked for in the executable only.** A server whose
  `request__start` is in a shared library is refused with the probes the
  executable carries. `runtime.custom` walks the libraries; `net.request` does
  not yet.
- **A guarded probe is refused.** The uprobe-multi link here does not raise a
  semaphore, so a probe behind one would never fire; the refusal says so.
- **A request is followed in the thread that declared it.** A server that
  hands a request to a pool thread must declare it again there, with the same
  id; the catalog's caveat says so. Nothing here tests a hand-off.
- **A `--pid` target is written and not exercised.** The probes are found
  through `/proc/<pid>/exe`; every scenario launches.
- **The join `net.request -> io.latency` on `req` does not run.** The request's
  one event is published at its end, after the block requests it covers, and
  the join engine refuses a rule at the first `to` event without the key --
  every block request outside a request. The second table says what the join
  would, per request; the join is stream T's filed need, seconded below.
- **What it costs.** The scheduler programs run on every context switch on the
  machine and do nothing for a thread with no request but look it up. Over one
  run the tool measured its own cpu at 0.85 ms against the server's 14.7 ms, on
  a server that does almost nothing but wait; on a server that computes, the
  share falls with it. Not measured on a switch-heavy machine.

---

## Files outside the stream's list

Two registration lines, without which nothing here exists, in files no stream
of this wave owns:

- `src/bpf/mod.rs`: `mod request;` and its `pub use`.
- `src/collect/net/mod.rs`: `mod request;` and the `net.request` factory.

---

## The suite

`make check` once, before the commit: clippy and fmt clean, and the tests red
on the first binary to fail, so one `cargo test --workspace --no-fail-fast`
followed: **1554 passed, 4 failed**, none in this stream's files.

| Test | Why it is red | Alone |
|---|---|---|
| `aggregation::two_emits_of_one_request_describe_the_same_events` | a float compared exactly; red on `main` too, filed by stream J | fails, as filed |
| `cli::the_session_indexes_hold_exactly_the_sessions_on_disk` | the two sessions below are not in the indexes yet | expected |
| `cross_compute::every_switch_of_the_task_is_credited_to_the_wakeup_that_released_it` | 574 wakeups against about 600 round trips under the full suite's load; not in the tally by name | passes |
| `scenario_mem::both_huge_page_backends_read_the_same_promotion_counter` | in the tally, finding 43 | passes |

`tests/vertical.rs` passed in every full and separate run: ten scenarios,
repeated five times alone and once under the full suite.

---

## Index rows, ready to paste

`examples/README.md` and `GUIDE.md`, in the `vertical-` section:

```
| `vertical-abandoned.yml` | Is a request the server never ended counted and named, or dropped? |
| `vertical-one-request.yml` | Where did one request's latency go, boundary by boundary, against injected delays? |
```
