# `net` — implementation notes

## Findings

### An interface needed to become a typed key

`/proc/net/dev` reports one line per interface, and the emit layer groups a
series by an event's first key. Without an interface key every interface fell
into one series. Adding `KeyKind::Iface` was not a presentation fix: a qdisc
delay, a driver drop and a link error are three sources whose only shared
identity is the interface, so it is a correlation key like the others. It
carries both the kernel index and the name, because a renamed interface is
still the same interface.

### A counter is not a measurement

The file holds totals since boot. What a run wants is what moved during it, so
the collector reports the increment between two reads. Two consequences, both
stated in the catalog rather than discovered later:

- The first read of a run is a baseline and produces no event. A run shorter
  than one poll interval measures nothing, and says so, instead of reporting
  the traffic since boot as though it had just happened.
- The run has to take a final read when it stops, or the last interval is
  dropped and every run under-reports by up to one tick. `stop()` closes the
  window.

A counter that goes backwards means the interface was removed and re-added.
That interval is counted as a failed read, not reported as a delta; the
alternative is a spike in the series that looks like traffic.

### `count` and `poll` are not interchangeable here

The mode x emit matrix refuses `rate` from a `poll`, which is right: `poll`
samples a level, where a rate means nothing. A counter aggregated over a window
is `count`, and that is what `net.bytes --mode count --emit rate` asks for. The
collector behaves the same either way; the mode says what the number means.

### Loopback counts everything twice

Every byte over `lo` is both received and transmitted, so the ground-truth test
asserts a floor of the transfer size and a ceiling that allows the doubling.

### A machine-wide source has one series, and it is the machine

`net.backlog` carries no key: the accept queue is the machine's, not an
interface's or a process's. The emit layer labelled such a series `<unkeyed>`,
which reads like something went missing. It now reads `system`, which is what it
is.

### Errors and drops are different failures

A bad frame and a full queue are counted apart everywhere in this family, and
summing them would hide which one happened. Same for `ListenOverflows` against
`ListenDrops`: an overflow is an application too slow to accept, a drop without
one is usually the SYN queue, and the two call for different fixes.

### What is not measured yet

`net.iface` reports the counters every driver keeps. The driver-specific list
`ethtool -S` prints needs the ethtool generic-netlink family, resolved by name
through `CTRL_CMD_GETFAMILY`. Until that exists, the `ethtool` requirement is
reported unverified rather than being answered as `NETLINK_ROUTE`, which would
be a different question's answer.

There is no ground-truth test that forces a link error. An accept-queue overflow
has one now -- a listener one deep given sixty-four connections -- and a veth
pair whose far end is never brought up gives the drops; what neither reaches is
a *malformed* frame, which is the other half of the family's rule that an error
and a drop are different failures.

## What the eBPF sources taught

Five sources landed in the sixth wave -- `net.tcp.lifetime`,
`net.tcp.retransmit`, `net.drops`, `net.qdisc` and `net.softirq` -- all through
`bpf:raw_tp`. What follows is what writing them found, in the order it matters
to whoever writes the next one.

### `Btf::member_offset` cannot see into an anonymous union, and the network stack is full of them

This is the finding of the wave and it shapes three of the five sources.

The reader resolves a member by name inside a *named* structure. `skc_daddr` and
`skc_dport` are members of an anonymous struct inside an anonymous union of
`sock_common`; `skb->protocol`, `skb->skb_iif` and `skb->dev` are members of an
anonymous union of `sk_buff`. None of them can be looked up at all.

A compile-time CO-RE relocation walks into anonymous members, which is how
`skb->protocol` resolves in any program built with clang. So this is a gap in
the reader rather than a property of the kernel, and it is the one thing another
family will hit the moment it reads a `bio` or a `page`.

What was done instead, and why it is not a workaround: the two TCP sources
report the **local** endpoint of each socket -- `inet_saddr` and `inet_sport`
are named members of `inet_sock` -- and `net.drops` reports the protocol of the
socket the drop belonged to. Each run says which fields it could not carry, so a
filter over one of them is a stated absence rather than a silent empty series.

Writing the offsets into the collectors was considered and refused. It is
precisely the staleness `docs/bpf-layer.md` says the layer exists to prevent,
and the failure mode is the worst shape a bug takes here: a plausible number
read out of the wrong field on a kernel nobody tested.

### A socket has three identities and none of them is always there

- **The cookie** is what the catalog keys this family on and what `net.tcp.rtt`
  reports. It is generated lazily, by `__sock_gen_cookie`, the first time
  anything asks -- and nothing asks on a socket's own account. A connection that
  opens and closes with nothing dumping it has none.
- **The inode** exists from the moment a socket has a file, and is what
  `/proc/<pid>/fd` shows as `socket:[n]`. A socket taken off a listen queue has
  no file until `accept()` gives it one, which is *after* it is established; and
  `tcp_close` calls `sock_orphan` before the state change, so the inode is
  already gone by `TCP_CLOSE`. The lifetime program therefore reads both ends
  and keeps whichever exists.
- **The pointer** is the only one always available, and it is the one that
  cannot leave the kernel: it is recycled within milliseconds, which is why
  `blk:rq` is scoped by an open timestamp and why nothing here keys on a `sock *`.

The practical consequence is worth stating plainly, because a session will be
written that depends on it: **a `sock:cookie` join between an eBPF source and
`net.tcp.rtt` works because `net.tcp.rtt` is running.** Its `sock_diag` poll is
what generates the cookies the eBPF events are keyed by.
`examples/net-connection-life.yml` is built on exactly that and says so.

### A connection has no process, at either end

Both ends reach `TCP_ESTABLISHED` in softirq context -- the passive end when the
final ACK arrives, the active end when the SYN-ACK does -- so the task current
at that moment is whichever one the interrupt landed on. A retransmission is
issued from a timer and is no better. `bpf_get_current_pid_tgid` returns a
number in all three cases and it is not the connection's owner.

So both TCP sources refuse a process target by name, with exit 3 rather than 1:
attributing a connection to a process needs a socket-to-process map, which is a
thing to build and not a thing this machine lacks. What they do carry is the
task current at the **close**, named `comm` and `pid`, with the run saying what
it is and is not: the closing process when an application closed the connection,
and whatever was running when the kernel closed it from a timer or on a reset.

### The kernel prints its own softirq table, so nothing here numbers the vectors

The vector a `softirq_entry` tracepoint reports is an index into
`softirq_to_name`, and `/proc/softirqs` is that table printed in that order. So
`net.softirq` reads the number of `NET_RX` off the running kernel and compares
against it in the program. The same file then gives the run an independent count
of its own work, which it reports:

```
degraded: the kernel's own /proc/softirqs counted 260 handler runs on these
          vectors over a window containing this run, and this run bucketed 260
```

That is worth copying. A source with any counter beside it should print the
comparison rather than leave it to a test, because a reader holding a number
wants to know whether it is all of them.

### A raw tracepoint is not namespaced, and eBPF cannot be loaded from inside one

Two facts that together decide how every namespaced network scenario has to be
arranged.

`bpf()` checks `CAP_BPF` against the *initial* user namespace, so a binary
carrying file capabilities loses them inside `unshare --user`: `omn` run there
answers "no viable backend" and names both capabilities. But a program attached
in the initial namespace sees events from every namespace, including packets
queued on a device this process cannot name.

So the workload goes inside and `omn` stays outside -- the opposite of the four
older `net` scenarios, which re-run *themselves* inside the namespace. Any
family wanting an exact count of anything network-shaped from an eBPF source
wants `workload_in_a_namespace` in `tests/scenario_net.rs` rather than
`rerun_inside_a_namespace`.

It also means an interface index on an event may belong to another namespace,
which is why `net.qdisc` reads the device's *name* in the kernel instead of
resolving the index afterwards.

### Loopback has no queueing discipline

`lo` is `noqueue` on every machine: the transmit path calls the driver directly
and neither qdisc tracepoint fires. A `net.qdisc` run on a machine whose only
live link is loopback measures nothing, correctly, and the collector says so in
its own notes -- an empty histogram with no explanation reads as a broken
collector, and this is the commonest way to get one.

## Status

Implemented, with ground truth:

| Source | Backend | Ground truth |
|---|---|---|
| `net.bytes` | `fs:proc` | 16 MiB pushed through loopback, asserted against the counters |
| `net.packets` | `fs:proc` | same file, same discipline |
| `net.tcp.rtt` | `netlink` | a loopback connection found in the dump, keyed by its cookie, round trip under 0.1 s |
| `net.iface` | `fs:proc` | loopback reports zero errors, drops, collisions and carrier losses |
| `net.backlog` | `fs:proc` | a one-deep accept queue given 64 connections, asserted to move the counters |
| `net.tcp.lifetime` | `bpf:raw_tp` | a connection held open 300 ms carrying 4096 bytes, found by both its ends, its inode checked against `/proc/<pid>/fd` |
| `net.tcp.retransmit` | `bpf:raw_tp` | `Tcp: RetransSegs` in a namespace whose only connection is the scenario's |
| `net.drops` | `bpf:raw_tp` | 512 datagrams to a closed port, and `Udp: NoPorts` |
| `net.qdisc` | `bpf:raw_tp` | 50 ms injected by `netem`, recovered from `total_ns` as 50.0018 ms |
| `net.softirq` | `bpf:raw_tp` | `/proc/softirqs`, read around the run, bounding it exactly |
| `net.tcp.connect` | `bpf:fentry` | twenty handshakes, five refusals and one dropped SYN, each found with its outcome, its port, its errno and the cookie `SO_COOKIE` gives the program's own socket; the dropped SYN at the kernel's one-second initial RTO |
| `net.socket` | `bpf:fentry` | 2001 sends on one socket, byte for byte, on the cookie `SO_COOKIE` gives it; a receive that waited 300 ms for its peer, at the length the program timed |

Blocked, and on what:

| Source | Blocked on |
|---|---|
| `net.capture` | `CAP_NET_RAW`. `AF_PACKET` is refused at uid 1000, so a collector here could not be given a ground truth, which is the standard this family is held to. The probe reports it as a privilege to grant, not a kernel to change. |
| `net.hwts` | Owning the socket. `SO_TIMESTAMPING` is set per socket, so reaching an arbitrary process's sockets means the preload shim. It also needs a NIC with a PHC; loopback has none. |
| `net.xdp` | Privileges, and an XDP program to attach. |

## Needs from the shared layer

**Two of these are blocking now**, and both are written out with the exact
signature wanted under `## Needs from another stream` in
`docs/open-findings.md`:

- **A member lookup that walks into anonymous members**, which is what a real
  CO-RE relocation does. Without it a connection's peer, a dropped packet's
  protocol and its interface are unreachable, and three sources in this family
  ship without filter fields their catalog entries declare. `io` and `mem` will
  want it for `bio` and `page` the moment they read one.
- **An enum reader for the kernel's BTF.** `net.drops` reports
  `enum skb_drop_reason` as a number because the names are in a place this build
  parses and cannot query.

Not blocking, but duplicated because of it:

- `read_field`, the per-cpu drop counter and the unrolled `log2` bucket are now
  written twice, in `src/bpf/runqlat.rs` and `src/bpf/net_program.rs`. They are
  general and belong beside `insn.rs`.
- `insn::helper` does not name `bpf_get_current_comm`, so `net_lifetime.rs`
  declares the number itself.

Ahead:

- `topk` emit, for `net.bytes --emit topk` across interfaces (G5).
- A netlink layer for `sock_diag`, `rtnetlink` and `ethtool`. It is family-local
  in `collect/net/netlink.rs` for now; if a second family needs it -- `io` will
  -- it should move up to `src/netlink.rs`.
- The ethtool generic-netlink family, resolved by name through
  `CTRL_CMD_GETFAMILY`, for the driver-specific counters `net.iface` currently
  cannot reach.

### The socket cookie was the right key, and it exists only if something asks

`sock_diag` reports a per-socket cookie, and it is exactly the `sock:cookie` the
catalog names as the key that survives softirq processing. `net.tcp.rtt` keys
its series on it and the eBPF sources read the same field, so the two join
without a translation layer.

What was not known when that was written is that the field is **empty until
something asks**: `__sock_gen_cookie` assigns a cookie the first time anything
calls for it, and nothing in the kernel calls for it on a socket's own account.
So the join works *because* `net.tcp.rtt` is in the session -- its dump is what
generates the cookies -- and a connection that opens and closes with nothing
looking at it carries no socket key at all. The eBPF sources emit no key rather
than a key of zero, which would join every anonymous connection to every other,
and the run says how many were in that state.

### Only established connections have a round trip

A listening or closing socket reports zero. Emitting that as a measured latency
of zero would be a lie with a number attached, so those sockets are skipped and
the catalog says why.

### What the RTT is, and is not

It is the kernel's smoothed estimate — what the stack currently believes about
the path, which is what drives its own congestion decisions. It is not a
measured packet round trip. `net.hwts` is the source that splits real latency
into NIC, kernel and application segments.

---

## The two sources over fentry

The eighth wave's: `net.tcp.connect` and `net.socket`, on the attach kind this
family had been waiting on -- `BPF_PROG_TYPE_TRACING` with an `attach_btf_id`,
in `src/bpf/fentry.rs`. `docs/families/io.md` has the attach kind itself and the
three losses it found; what follows is what the two `net` sources needed.

| Source | Opened at | Closed at | Shape |
|---|---|---|---|
| `net.tcp.connect` | `tcp_v4_connect`, `tcp_v6_connect` | `tcp_finish_connect` (established), `tcp_done_with_error` (refused or timed out), the connect's own error return (failed) | one record per handshake |
| `net.socket` | `sock_recvmsg`; `inet_sendmsg`, `inet6_sendmsg`, `unix_stream_sendmsg`, `unix_dgram_sendmsg` | the same functions' exit halves | counted in the kernel, a row per `(task, op, socket, log2 bucket)` |

### The cookie is generated, not found

`net.tcp.lifetime` reads `skc_cookie`, which is zero on any socket nothing has
dumped -- finding 11 of `docs/scenarios/net.md`, and the reason
`examples/net-connection-life.yml` carries `net.tcp.rtt` for nothing but its
`sock_diag` poll. A tracing program can call `bpf_get_socket_cookie` on a
`struct sock *`, which **assigns** the cookie if nothing has yet. Both fentry
sources do, so their key is the number `SO_COOKIE` and `ss -e` report for the
same socket -- checked: 20 of 20 handshakes, and the one socket of the send
scenario, against `getsockopt(SO_COOKIE)` asked by the program of its own
sockets.

It changes what the rest of the family can do, too. A connection whose
handshake `net.tcp.connect` saw has its cookie set by the time it closes, so
`net.tcp.lifetime` reads it there, and the two join on `sock:cookie` **with
nothing else in the session**: `examples/net-handshake-to-close.yml`, measured
at `10 of 10 net.tcp.lifetime events attributed (100.0 %)`, evidence `KEY`.

The server-side closes of the same connections carry no cookie -- a socket
taken off a listen queue never called `connect`, and nothing asked -- and the
join engine refuses a whole rule at the first `to` event without its key, as a
rule that does not fit its sources. The session filters on `cookie > 0` and
says so, `net.tcp.lifetime` declares the `cookie` filter field its events
always carried, and the request that such an event be counted as
unattributable instead is filed for the join engine's stream.

### A handshake closes in someone else's context

The SYN-ACK is processed in softirq, on whatever cpu took the packet, so at the
close the current task is whoever the interrupt landed on. The stamp is
therefore keyed by the **socket** and carries the connecting task, its thread
id and its name with it, and every event names the task that called `connect`
-- never the bystander. Every handshake ends in one of three outcomes and each
is its own event: `established`, `refused` with the errno the application was
given (111 for a RST), or `failed` when the connect call returned an error
before a SYN left. A handshake a program abandons while still in `SYN_SENT`
closes through none of them and is counted at the end as still in flight.

**A dropped SYN is the injected delay this source can be checked against.** A
listener whose accept queue is full drops the next SYN, and the client resends
it after the kernel's initial retransmission timeout, one second. With a
backlog of zero, one queued connection is a full queue; the scenario's slow
handshake comes back above a second and inside the interval the program timed
around its own `connect`, while twenty handshakes to an idle listener come back
under a tenth of that. The catalog's caveat now says it in the reader's terms:
a handshake near one second is a lost SYN, not a slow peer.

### The send path is not `sock_sendmsg`

The catalog named `sock_sendmsg` and `sock_recvmsg`, the obvious mirror pair,
and the first run found every receive and **none of 2000 sends**. Every syscall
path -- `send`, `sendto`, `sendmsg`, `write` on a socket -- calls
`__sock_sendmsg`, which is not a symbol at all on this kernel: the compiler
inlined it everywhere, and `sock_sendmsg` is the out-of-line copy the kernel's
own senders use. What each of those paths does call out of line is the
family's `sock->ops->sendmsg`, and the four families that carry nearly all
traffic share one prototype, so sends are timed at `inet_sendmsg`,
`inet6_sendmsg`, `unix_stream_sendmsg` and `unix_dgram_sendmsg` and receives
stay at `sock_recvmsg`. **The two directions are therefore not symmetric**: a
send on a netlink or packet socket is not timed and a receive on one is. Every
run says so, and so does the caveat. Half the scenario's sends are `write(2)`
on the socket, which is the check that the send path is the right one.

### What was measured

- **2001 sends and 8,192,001 bytes** on the client's socket, and the same bytes
  received on the server's, each on the cookie its own `getsockopt(SO_COOKIE)`
  reports. Both numbers came out one more than the scenario first expected --
  the byte that ends the exchange is a send too -- and both times the tool was
  right and the expectation wrong.
- **A receive that waited 300 ms** for its peer, timed by the program around
  its own call, lands inside that interval in `net.socket`.
- **Twenty established, five refused with errno 111**, each on the port the
  program connected to, and one handshake behind a dropped SYN at over one
  second.
- **The loss paths.** Five thousand non-blocking connects parked in `SYN_SENT`
  against a full listener is more than the 4096 handshakes the pending map
  holds. Measured: **903 dropped** -- five thousand, less the 4096 the map
  holds, less the one handshake the queue let through, which established and
  freed its slot -- and 4096 reported still in flight at the end. A hundred threads each sending one byte on a thousand socket pairs is
  a hundred thousand histogram rows, and the rows plus the drops are never
  fewer than the sends.

### What these two cannot do

- **`net.socket` sends are timed for inet, inet6 and unix sockets only**, for
  the reason above.
- **A receive's latency includes the wait for data.** A blocking `recv` that
  waited a second for its peer is a second here, and is not a slow network;
  the caveat says it.
- **`net.tcp.connect` sees the client side only.** A server's accepted sockets
  never called `connect`; `net.tcp.lifetime` is where they appear.
- **A cgroup target is refused** with exit 3, naming
  `bpf_get_current_cgroup_id`.
- **Their `bpf:kprobe` backends have no collector**, which is what
  `examples/net-not-built.yml` now pins to show exit 3: the two sources the
  file used to ask for collect through `bpf:fentry`, which was the day its
  own comment said it would stop being an example.
- **The exit half of a call in flight can be lost** when another tracer
  attaches to the same function, which `io.md` has in full. Counted, not
  prevented; a long blocking `recv` is the likeliest casualty.

### Sessions

- `examples/net-fentry-sweep.yml` -- every emit both declare, in every mode the
  matrix allows.
- `examples/net-handshake-to-close.yml` -- the join above.
- `examples/net-not-built.yml` -- rewritten to pin the unbuilt `bpf:kprobe`
  backends.

The first two need rows in `examples/README.md` and `GUIDE.md`, which are the
coordinator's.
