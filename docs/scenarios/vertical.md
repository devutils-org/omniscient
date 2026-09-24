# The vertical case — one request, boundary by boundary

**Where did one request's latency go?** `ROADMAP.md` phase 4 calls this the
real milestone: a request that reads a file and sends it back, decomposed
across the boundaries it crosses, every line carrying its evidence, checked
against a server whose delays the test injected. This document was stream V's
deliverable in the ninth wave, and is stream N's in the tenth: the two network
rows, `net_rx` and `net_tx`, which the ninth wave left as the next step.

| Session | What it shows |
|---|---|
| `examples/vertical-one-request.yml` | five requests to a real server with injected delays -- a lock, cold reads, both, a client that pauses mid-request, a client that reads slowly -- each decomposed by the tool into six rows, per request |
| `examples/vertical-abandoned.yml` | a request the server declares and never ends, and one whose answer never leaves the stack: each counted and named, and kept out of the next |

The checks are in `tests/vertical.rs`, fifteen scenarios. Every number below
was produced on the development machine: an AMD Zen 2 with 24 cpus, an NVMe
disk with the `none` scheduler, btrfs, kernel 7.2, with other streams' suites
running beside it.

**What the tenth wave holds, in one paragraph.** Both injections the brief
proposed, and both need no root. A client that sends half its request, sleeps
30 ms and sends the rest puts **30.14 to 30.27 ms** in `net_rx` against a pause
it measured at 30.06 ms; a client that reads 128 KiB through a 4 KiB receive
buffer puts **32.8 to 33.8 ms** in `net_tx`, of which 28.6 ms is the server's
send blocked on the window and 4.2 to 5.3 ms is the response draining after the
server had declared the request done. The last byte left the stack between
the moment the client made room for it and the moment it read it, **every run,
in absolute time on the kernel's clock**. The rows still sum to the client's
latency: **+0.06 % to +0.36 %** for the four requests whose answer fits the
window, **+3.1 %** for the slow reader, whose last receive buffer the client
reads after the server is done with it. Both rows are `KEY` -- read by the
socket's cookie -- and the table prints them `IDENTITY` until the engine can
say otherwise; that is filed, and it is the one thing here not yet right.

---

## What the tool says

`omn -f examples/vertical-one-request.yml`, the tenth wave's table for the
three requests that carry the network injections and one of the others:

```
decompose one-request  per req, in ns

  req:a004
    STATE        SOURCE             VALUE        KIND     EVIDENCE          AMOUNT   RESOLUTION   SHARE
    net_rx       net.request        net_rx_ns    traced   IDENTITY         30.09ms        exact  99.9 %
    cpu_on       net.request        oncpu_ns     traced   IDENTITY         23.31us        exact   0.1 %
    sched_wait   net.request        runq_ns      traced   IDENTITY             0ns        exact   0.0 %
    lock_wait    net.request        sleep_ns     traced   IDENTITY             0ns        exact   0.0 %
    block_io     net.request        disk_ns      traced   IDENTITY             0ns        exact   0.0 %
    net_tx       net.request        net_tx_ns    traced   IDENTITY             0ns        exact   0.0 %
    whole        net.request        span_ns      traced   IDENTITY         30.11ms        exact
    RESIDUE      whole - parts                                                +0ns        +-0ns

  req:a005
    net_rx       net.request        net_rx_ns    traced   IDENTITY         12.36us        exact   0.0 %
    cpu_on       net.request        oncpu_ns     traced   IDENTITY        247.20us        exact   0.7 %
    sched_wait   net.request        runq_ns      traced   IDENTITY         65.07us        exact   0.2 %
    lock_wait    net.request        sleep_ns     traced   IDENTITY             0ns        exact   0.0 %
    block_io     net.request        disk_ns      traced   IDENTITY             0ns        exact   0.0 %
    net_tx       net.request        net_tx_ns    traced   IDENTITY         32.88ms        exact  99.0 %
    whole        net.request        span_ns      traced   IDENTITY         33.20ms        exact
    RESIDUE      whole - parts                                                +0ns        +-0ns

  req:a001
    net_rx       net.request        net_rx_ns    traced   IDENTITY         18.23us        exact   0.0 %
    cpu_on       net.request        oncpu_ns     traced   IDENTITY         52.61us        exact   0.1 %
    sched_wait   net.request        runq_ns      traced   IDENTITY         13.62us        exact   0.0 %
    lock_wait    net.request        sleep_ns     traced   IDENTITY         40.06ms        exact  99.8 %  -> holder_tid=253000
    block_io     net.request        disk_ns      traced   IDENTITY             0ns        exact   0.0 %
    net_tx       net.request        net_tx_ns    traced   IDENTITY             0ns        exact   0.0 %
    whole        net.request        span_ns      traced   IDENTITY         40.14ms        exact
    RESIDUE      whole - parts                                                +0ns        +-0ns
```

The `net_rx` and `net_tx` rows say `IDENTITY` and should say `KEY`; the next
section says why, and `docs/open-findings.md` has the request under stream N.

The ninth wave's two tables, which the rows above extend -- its whole was the
life, `total_ns`, where it is now the span, `span_ns`:

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

And the tenth wave's two, over three consecutive runs of `tests/vertical.rs`:

| | injected | the tool | the reference | client latency | rows' sum |
|---|---|---|---|---|---|
| `req:a004` | a 30 ms pause between the two halves of the request | `net_rx` 30.146, 30.139, 30.273 ms | the client's own pause, 30.065 ms; its first byte stamped 7 to 15 us *before* the client's send returned | 30.217 ms | 30.184 ms (+0.11 %) |
| `req:a005` | 128 KiB through a 4 KiB receive buffer, 1 ms between reads | `net_tx` 33.059, 32.830, 33.824 ms: 28.6 to 28.8 ms blocked in the send, 4.2 to 5.3 ms draining after the end; `lock_wait` 0 | the server's send, 28.8 to 29.0 ms; the last byte left 1.07 ms after the client made room for it and 1.06 ms before it read it | 34.496 ms | 33.418 ms (+3.13 %) |

Five consecutive quiet runs put the gap between the client's latency and the
rows' sum at **+0.11 % to +0.39 %**, and the loaded one at +0.42 %, against the
roadmap's 10 %. The gap is positive
every time, as it has to be: it is the loopback in both directions, the accept,
the parse before the birth and the client's own wakeup, none of which is the
server's.

With the network rows, five consecutive runs of the tenth wave's session put
it at **+0.06 % to +0.36 %** for `a001` to `a004` -- smaller than before, because
the accept-to-birth stretch that was the gap is now `net_rx` -- and at
**+3.08 % to +3.16 %** for `a005`. That one is the slow reader's own: when the
last byte leaves the server's stack a receive buffer of it is still unread,
and the client reads it after one more pause. It is the client's time and in
no row, and it is the reason the client does not pause after its last read
(otherwise it was +6 %).

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
| `the_tool_decomposes_each_request_against_its_own_life` | the server's clock around the two probes | the whole is the request's span -- since the tenth wave, the life with the network on either side -- the total within that clock, the device row the tagged requests' sum |
| `a_request_that_never_ends_is_counted_and_named_and_does_not_leak` | a request the server abandons | "2 born, 1 ended", the abandoned one named with its thread, the second birth reported as a restart, and the next request whole |
| `a_program_that_declares_no_request_is_refused_by_name`, `a_mode_that_cannot_follow_a_request_is_refused_by_name` | -- | refused, naming the two probes and the mode |
| `without_the_capabilities_the_session_says_which_it_needs` | a copy of the binary, which carries no file capabilities | refused, naming `CAP_BPF` |

**What the roadmap asked for and this machine cannot give**: `tc netem` and
`dm-delay` need root. The network delay would have gone into `net_rx`/`net_tx`
and the device delay into `block_io` at the device end; the lock and the cold
read are the two injections this machine can make without root, and they are
the two rows the tests hold. No scenario pretends to have the other two.

---

## The network rows -- the tenth wave

`src/bpf/request_net.rs` has the programs and the long version.

### What each row is

**`net_rx`** is from the request's first byte reaching the server's socket to
the serving thread taking it. Two stretches, one on each side of the birth:

- *before* it, from the first byte's arrival to `request__start` -- where a
  client that pauses mid-request puts its pause, because a server cannot
  declare a request it has not read;
- *inside* the life, every interruptible sleep in `sk_wait_data` on that
  socket -- a server that declares the request at its headers and reads a body
  after.

**`net_tx`** is from the response leaving the thread to it leaving the stack:

- *inside* the life, every sleep in `sk_stream_wait_memory` on the socket --
  the send blocked because the send buffer was full, which against a slow
  reader means the window was closed;
- *after* the end, from `request__done` to the moment `snd_nxt` reached the
  `write_seq` the end saw: the last byte queued by then was transmitted. A send
  returns when the bytes are queued, not when they are out.

The six rows tile the **span**, first byte in to last byte out, exactly:
`span_ns = rx_queued_ns + total_ns + tx_drain_ns`, and the two waits are taken
out of `sleep_ns` where they used to sit. So the residue is zero by
construction, as before, and says nothing; what says something is which row
each delay lands in, and the absolute stamps below.

### Where the socket comes from: the server says so

The ninth wave left two ways: a second argument on the two probes, or the
thread's last receive. **The second argument**, `request__start(id, fd)` and
`request__done(id, fd)`, the descriptor the request was received and answered
on -- for the reason the birth itself is declared. A thread that reads from a
backend after its client (a proxy, a cache fill) would have its last receive
on the wrong connection, and nothing in the numbers would say so. The programs
walk `current->files->fdt->fd[fd]->private_data->sk`, checking the descriptor
against `max_fds` and that the `struct socket` points back at the same `struct
file` -- which a descriptor that is not a socket fails. A probe with one
argument is still a request, with no network rows, and the run says why.

### Why the rows are KEY

Every other row is read off the serving thread, which carries the id. Nothing
on the receive or transmit path runs there: on loopback the first byte is
queued in softirq inside the *client's* send, and the last byte leaves when the
client's read opens the window, in the client's context. What ties those
moments to the request is the socket's cookie, matched between the program
that saw the byte and the probe that named the socket -- a key both sides
agree on, not an identity one of them propagated. `KEY`, which is also what
`ROADMAP.md`'s table says of boundaries 1 and 10.

**The table prints `IDENTITY`**, because the decomposition rates every row of a
`by: req` table by that key alone, and there is no way for a part to say it was
read through another. The request is under stream N in `docs/open-findings.md`,
with the signature: a `via:` on `Part`, and the row rated the weaker of the
two keys. When it lands, the two parts in `examples/vertical-one-request.yml`
take `via: sock` and one assertion in
`the_network_rows_are_read_by_the_socket_the_server_accepted` flips. Until
then the rows are over-rated, which is the direction the ladder must not err
in, and this is where it is said.

### The mechanism

Six fentry programs, attached before any probe, only when a probe names a
socket -- they run on every TCP socket of the machine:

| Program | Function | What it does |
|---|---|---|
| arrival | `tcp_data_ready`, entry | asks for the socket's cookie (which assigns it), stamps the oldest byte no request has claimed, unless a request is live on the socket |
| transmit | `tcp_event_new_data_sent`, **exit** | when an ended request armed the socket and `snd_nxt` has reached its target, claims the arming and publishes a drain record |
| rx wait | `sk_wait_data`, entry and exit | marks the serving thread as waiting on its request's own socket |
| tx wait | `sk_stream_wait_memory`, entry and exit | the same, for room to send |

Three things were not obvious:

- **A uprobe cannot ask for the cookie.** `bpf_get_socket_cookie` takes a BTF
  pointer only tracing programs are given. The probes read
  `skc_cookie` instead, which is zero until someone asks -- so the arrival
  program asks, and a socket whose first byte was not seen has no cookie to
  read. Its request says so rather than carrying a zero.
- **`snd_nxt` moves at the end of the function that sends**, so the transmit
  program is an fexit: at the entry the last segment's `snd_nxt` is not written
  yet, and a request ending at that instant would never be drained.
- **The end and the last transmission race**, on two cpus. The end arms the
  socket, then reads `snd_nxt`; the transmit path writes `snd_nxt`, then reads
  the arming. Each write is an atomic read-modify-write, so at least one side
  sees the other, and the drain is claimed by compare-and-exchange: exactly one
  of them stamps it.

A drain can reach the ring before the end record it belongs to, so the
collector joins them by `(id, end)` in either order, and holds an ended request
until its drain arrives. Its event is still stamped at its end.

### The ground truth, and why it needs no root

`tc netem` would put a delay into the network where the tool never sees it
happen. A client can put one exactly where the tool does, and measure it on its
own clock -- which is `CLOCK_MONOTONIC`, the kernel's -- so the tool's stamps
are checked in absolute time, not as durations:

| Scenario | Reference | Bound |
|---|---|---|
| `a_pause_in_the_middle_of_the_request_lands_in_net_rx` | the client's stamps around its two sends and its pause | first byte ≥ the first send's start and ≤ the pause's end; `net_rx` ≥ second send − first byte (the birth cannot precede the second half), **no slack**; `net_rx` ≥ 90 % of the pause; every other request < a tenth of it |
| `a_slow_reader_holds_the_response_in_net_tx` | the client's read stamps and its `SO_RCVBUF`; the server's clock around its send | the last byte left **after** the start of the read after which less than a receive buffer was unread, and **before** the client's last read ended -- no slack; the send's wait ≤ the server's send clock; `net_tx` ≤ send start to last byte ≤ `net_tx` + `cpu_on` + `sched_wait`; `lock_wait` < 1 ms; every other request's `net_tx` < 1 ms |
| `the_network_rows_are_read_by_the_socket_the_server_accepted` | `getsockopt(SO_COOKIE)` on the accepted connection, by the server | the event's `sock` key equal, every request; the evidence as it stands |
| `without_the_socket_a_wait_on_it_is_a_sleep_and_the_run_says_so` | the server's send clock; the client's tid | through `bpf:usdt:threads`: no network rows, the degradation named, the send's wait in `sleep_ns` and the client named as its holder |
| `a_request_that_never_ends_is_counted_and_named_and_does_not_leak` (extended) | a client that closes without reading 16 KiB | the request is an event with `net_rx` and **no** `net_tx`; "1 response had not left the stack" |
| `the_segments_sum_to_the_latency_the_client_measured` | the client's clock | all five within 10 %, with six rows |

Why the lower bound on the drain holds: before the read after which less than
`SO_RCVBUF` bytes were left, more than a receive buffer of the answer was
unread, and the server cannot transmit past the right edge of the window the
client advertised -- which is at most a receive buffer beyond what it had read.

**The slow reader is the fallback's worst case, and the test shows it.**
Through `bpf:usdt:threads` the same request's send wait is `sleep_ns`, printed
as `lock_wait`, with `holder_tid` naming the *client* -- whose read opened the
window and woke the send. Specific, plausible and wrong, which is what the
socket rows exist to prevent.

### What it costs

The first backend declares its four `fentry:` requirements, so the resolver
builds the BTF function index before the run: 100 ms in a debug build, which
was being charged to the run when nothing asked for it earlier. The programs
load in `attach()` and detach when the collector is dropped, as every other
eBPF source here does: detaching six trampolines is **60 ms of system time**,
and while it happened in `stop()` the run reported the source at 2 500 % of a
server's cpu. With both moved, the source costs **1.3 ms** against the
server's 3.3 ms over the session's five requests -- mostly the scheduler
programs, which run on every switch of the machine, and the arrival and
transmit programs, which run on every TCP segment of it. Not measured on a
machine whose network is busy.

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
today's behaviour so the day it changes is seen. *Landed by the coordinator
after the ninth wave: `req:a001` has a `block-io` table now, all of it residue
-- zero here, since it waited on no disk.*

**5. Every row says KEY.** The decomposition engine writes `Evidence::Key` on
every row, whatever it is read by. A row read per `req` is read by an identity
the upstream event propagated, and the join engine already rates a join on
`req` IDENTITY; the decomposition does not yet. The tables above are therefore
under-rated, never over-rated. Filed for `src/join.rs`. *Landed after the ninth
wave: a row read per `req` says IDENTITY -- every row, which the tenth wave's
network rows then made too many; see "Why the rows are KEY" above.*

---

## The roadmap's ten boundaries, and where this build is

| # | Boundary | `ROADMAP.md` | This build | Evidence |
|---|---|---|---|---|
| 1 | NIC → softirq → TCP | `sock:cookie` | **`net_rx`: the first byte stamped at `tcp_data_ready`, by cookie**; on loopback there is no NIC, and the receive runs in the client's send | KEY (printed IDENTITY, filed) |
| 2 | socket ready → thread woken | WAKEUP | inside `net_rx`: the thread waits for the rest of its request before the request exists, and after the birth its waits in `sk_wait_data` are `rx_wait_ns` | -- |
| 3 | request entry | `bpf_task_storage`, IDENTITY | **`request__start`, the id in the thread's storage** | IDENTITY |
| 4 | lock wait | WAKEUP → holder | **`lock_wait`, split from the runnable wait at `sched_waking`, `holder_tid` named** | the row says KEY (finding 5); the holder is WAKEUP |
| 5 | `read()` → cache miss | `file:dev+ino` | not joined: the id makes it unnecessary for the block row | -- |
| 6 | FS → block submission | re-read task storage at `block_rq_issue`, IDENTITY | **`io.latency` carries `req:<id>`, read at the issue** | IDENTITY |
| 7 | submit → completion | `blk:rq@ts` | `io.latency` on `blk:rq@open_ts`, unchanged | KEY |
| 8 | completion → wakeup | WAKEUP | the `block-io` residue: the kworker's hop, measured, not joined | -- |
| 9 | response | `req:id` → `sock:cookie` | **`request__done(id, fd)`: the socket the end names must be the birth's**; its waits in `sk_stream_wait_memory` are `tx_wait_ns` | KEY |
| 10 | TCP TX → NIC | `SO_TIMESTAMPING` | **`net_tx`'s drain: `snd_nxt` reaching the end's `write_seq`, at `tcp_event_new_data_sent`** -- the software moment the last byte leaves the stack, which on loopback is its arrival at the peer | KEY (printed IDENTITY, filed) |

The ninth wave's closing paragraph here named `net_rx` and `net_tx` as the
two rows this build did not have, and the design for them: the connection's
cookie at the birth and the end, a second argument on the same two probes.
That is what was built. `SO_TIMESTAMPING` is not used: its stamps come back to
the program that asked for them, on its error queue, and the tool cannot read
another process's; what the kernel's own `snd_nxt` says is the same moment on
loopback, where there is no NIC to stamp a later one.

---

## Limits, stated

- **Probes are looked for in the executable only.** A server whose
  `request__start` is in a shared library is refused with the probes the
  executable carries. `runtime.custom` walks the libraries; `net.request` does
  not yet.
- ~~**A guarded probe is refused.**~~ **Lifted in the tenth wave.** The link
  hands the kernel each site's semaphore to raise for as long as it lives;
  `a_probe_behind_a_semaphore_is_raised_and_followed` builds the server with
  both probes behind one and holds the server's own read of it: raised under
  the tool, zero without it.
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
  share falls with it. Not measured on a switch-heavy machine. The socket
  programs' cost is in the section above.

And the network rows' own, each stated rather than tested around:

- **Pipelined requests.** `net_rx` starts at the first byte to arrive after the
  socket's previous request ended; bytes that arrive while a request is live
  are that request's. Two requests whose bytes arrive together, before the
  first is born, give the second no `net_rx` rather than the first's. Nothing
  here pipelines, so nothing tests it.
- **A keep-alive connection draining one answer while the next request is
  born** re-arms the socket for the new request and the old drain is not
  followed: its request ends the run without `net_tx`, and is counted.
- **The socket map is an LRU of 16384 sockets**, because the arrival program
  sees every socket on the machine and cannot tell the server's from others
  before the probe names it. On a machine with more sockets receiving between
  a request's first byte and its birth, the entry can be evicted, and the
  request says its first byte was not seen.
- **A byte queued while the socket is locked by its reader** is stamped when
  the reader releases the lock and the backlog is processed, not when it
  arrived. On loopback, with the reader asleep in `sk_wait_data` -- which
  releases the lock -- this does not happen; on a busy server it would shorten
  `net_rx` by the time the segment spent in the backlog.
- **TCP only.** `tcp_data_ready` and `tcp_event_new_data_sent` are TCP's; a
  request over UDP or a unix socket names its descriptor and gets no network
  rows, with the reason.
- **`rx_wait_ns` is built and not exercised.** No scenario declares a request
  before it has read all of it, so no request here waits in `sk_wait_data`
  after its birth; the pause of `req:a004` is before it. The routing is the
  same code as `tx_wait_ns`'s, which the slow reader does exercise, and
  `a_sleep_on_the_socket_is_not_counted_as_a_sleep` holds both in the
  instruction stream -- which is not the same as a measurement.

---

## Files outside the stream's list

Ninth wave: two registration lines, in `src/bpf/mod.rs` (`mod request;` and its
`pub use`) and `src/collect/net/mod.rs` (`mod request;` and the factory).

**Tenth wave: none.** `src/bpf/request_net.rs` is a child of `request.rs`,
declared there with `#[path = "request_net.rs"] mod net;`, so it needs no line
in `src/bpf/mod.rs`, and what the collector needs from it is reached through
methods on `RequestRecord` and `RequestProgram`, which `mod.rs` already
re-exports. The second backend's factory is in `src/collect/net/mod.rs`, which
is this stream's. The one request outside the stream's files is the engine's
`via:`, filed.

---

## The suite

### Tenth wave

`cargo test --test vertical`: **15 passed**, five consecutive runs alone, the
numbers above from three of them. `cargo test --lib bpf::request` 16 passed,
`cargo test --lib collect::net::request` 8 passed, `cargo test --test
scenario_net` 37 passed, `cargo test --test cli` 18 passed -- the session-index
test is green, because no session was added: the two this stream changed were
already in both indexes.

`make check` once before the commit: clippy and fmt clean, and the tests red at
the first binary to fail, `aggregation`, so one `cargo test --workspace
--no-fail-fast` followed: **1632 passed, 4 failed**, none in this stream's
files, and each failure rerun alone once:

| Test | Why it is red | Alone |
|---|---|---|
| `aggregation::two_emits_of_one_request_describe_the_same_events` | a float compared exactly; red on `main` too, filed by stream J (`docs/open-findings.md`) | fails, as filed |
| `cross_compute::three_states_at_once_still_match_the_kernel_and_their_residue_is_printed` | under the full suite's load; not in the tally by name | passes |
| `scenario_io::every_sync_is_one_event_on_the_file_it_named` | under the full suite's load; not in the tally by name | passes |
| `scenario_mem::a_walk_that_fits_in_l1_is_never_timed_and_one_that_does_not_waits_on_memory` | a cache measurement on a machine four streams share; not in the tally by name | passes |

`tests/vertical.rs` passed in the full run too. The three that passed alone are
new to the tally and belong in it; none touches `net.request`.

### Ninth wave

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

**Tenth wave: no session was added.** The two rows exist in `examples/README.md`
and `GUIDE.md`; what the sessions show grew, and the rows can say so -- the
coordinator's call:

```
| `vertical-one-request.yml` | Where did one request's latency go, boundary by boundary -- network included -- against injected delays? |
| `vertical-abandoned.yml` | Is a request the server never ended, or an answer that never left, counted and named, or dropped? |
```

For the coordinator beside the rows: `STATE.md`'s phase-4 line and
`ROADMAP.md`'s exit criteria say the network rows wait on root; they do not any
more (the ground truth above is a client's, not `tc netem`'s), and
`dm-delay` is the one injection left. `CATALOG.md` has no row for
`net.request`; its two backends are `bpf:usdt` and `bpf:usdt:threads`.

Ninth wave:

`examples/README.md` and `GUIDE.md`, in the `vertical-` section:

```
| `vertical-abandoned.yml` | Is a request the server never ended counted and named, or dropped? |
| `vertical-one-request.yml` | Where did one request's latency go, boundary by boundary, against injected delays? |
```
