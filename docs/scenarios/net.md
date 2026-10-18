# `net` — what is worth measuring, and how you know the answer is right

Twelve of the fifteen `net` sources have a collector. This document is what to
measure with them, what the numbers are checked against, what this family
cannot check and why, and what the checking found.

The sessions are in `examples/net-*.yml` and the checks in
`tests/scenario_net.rs`. Thirty scenarios, about nine seconds in parallel. Four of them are over a second each and none of
them is over four: the injected round trip waits out a connection's first two
round trips before the kernel has an estimate to report, the capture and the
two-mechanism comparison each hold a ring open for most of a second before
anything is sent, and every `count` run observes a whole second because that is the cadence
of the mode. The rest are tenths.

---

## What the family can serve

| Source | Backend | Modes | Emits | Where its ground truth comes from |
|---|---|---|---|---|
| `net.bytes` | `fs:proc` | poll, count | rate, timeline, summary | bytes pushed through loopback |
| `net.packets` | `fs:proc` | poll, count | rate, timeline, summary | datagrams counted by the sender |
| `net.iface` | `fs:proc` | poll, count, snapshot | rate, raw, timeline, summary | a link whose far end is down |
| `net.backlog` | `fs:proc` | poll, count, snapshot | rate, raw, timeline, summary | a one-deep accept queue |
| `net.tcp.rtt` | `netlink` | poll, trace | raw, hist, timeline, summary | a delay injected by `netem` |
| `net.hwts` | `netlink:ethtool` | poll, snapshot | raw, timeline, summary | two clocks, and the passage of time |
| `net.capture` | `af_packet` | trace | raw, timeline | frames the scenario sent |
| `net.tcp.lifetime` | `bpf:raw_tp` | trace | hist, topk, raw, timeline, summary | a connection held open for a chosen time, carrying a chosen number of bytes |
| `net.tcp.retransmit` | `bpf:raw_tp` | trace, count | rate, topk, raw, summary | the kernel's own `Tcp: RetransSegs`, over a link removed on purpose |
| `net.drops` | `bpf:raw_tp` | trace, count | rate, topk, raw, summary | a chosen number of datagrams sent where nothing is listening, and `Udp: NoPorts` |
| `net.qdisc` | `bpf:raw_tp` | trace | hist, raw, summary, timeline | a delay injected by `netem` |
| `net.softirq` | `bpf:raw_tp` | trace, count | hist, rate, raw, summary | the kernel's own `/proc/softirqs` |

`net.bytes` also declared `topk`, and no mode it declares could produce one. That
is the first entry under **Defects found**, and it is fixed: the emit was out of
the entry until `count` mode could rank what it counted, which it can since the
thirteenth wave, and `topk` is back.

The other three sources are vocabulary: `net.tcp.connect`, `net.socket` and
`net.xdp`. The first two wait on **fentry** -- BTF-typed attach, which the eBPF
layer does not have -- and the third on an XDP program to load. Five sources
came off that list in the sixth wave, through the `bpf:raw_tp` backend the
catalog pass put ahead of their classic one; the three that are left need a
mechanism rather than a collector. `examples/net-not-built.yml` is what asking
for one looks like.

**Every one of the five needs `CAP_BPF` and `CAP_PERFMON`**, which `make` puts
on the binary and every `cargo build` takes off again. A machine that does not
grant them answers 1 and names both, and every scenario below says so on stderr
and stands down. A green suite on such a machine is not evidence about any of
this.

---

## Scenarios, by the question they answer

### Is this path slow, and by how much?

`examples/net-injected-rtt.yml`. **The strongest statement this family makes
about its own numbers.**

A network emulator on loopback delays every packet by twenty-five milliseconds
in each direction, so a round trip is fifty. The number is chosen in the
scenario, produced by a queueing discipline, and reached by `net.tcp.rtt`
through a netlink dump that knows nothing about any of it:

```
  timeline
    sock:94457
      +0ns        50.04ms
      +200ms      50.03ms
      +400ms      50.04ms
      +600ms      50.03ms
      +800ms      50.04ms
      +1s         50.04ms
```

Both modes the source declares are asked over the same window and both land on
the same fifty milliseconds. `trace` here is read at the polled cadence — the
netlink backend has no per-update hook — so what the mode changes is what the
numbers mean, not how they were obtained, which is the family's own note in
`docs/families/net.md` and is visible in this scenario rather than only written
down.

What it does not check: that fifty milliseconds is a *measured* round trip. It
is the kernel's smoothed estimate, which is what drives the stack's own
decisions and is not the same thing. `net.hwts` is the source that would split a
real latency into NIC, kernel and application segments, and it needs a backend
nobody has written.

### How much traffic crossed this link?

`examples/net-two-mechanisms.yml`, and it is also this family's answer to the
backend comparison.

**No `net` source has two implemented backends**, so there is no comparison to
make inside a chain — the brief says to say so rather than invent one. That is
still true after the sixth wave: each of the five eBPF sources has exactly one
implemented backend and one declared behind it, so what a forced `bpf:tp`
establishes is that the chain is real — the resolver knows about the second
road, names it, and says which kind of obstacle is in the way. On this machine
it is always the same obstacle, and it is the machine's rather than the build's:
a classic tracepoint's numeric event id lives in a file under tracefs that uid
1000 cannot read. What
there is instead is two *mechanisms* measuring one wire over one window:
`/proc/net/dev`, which is the kernel's own per-interface accounting, and
`AF_PACKET`, which is every frame copied out of a ring. They were written by
different people for different reasons, which is what makes their agreement
worth something.

Two thousand datagrams of a hundred bytes, in a network namespace where the only
traffic is the scenario's, and loopback carries every one of them twice:

```
volume   [('iface:lo', 512000.0)]      4000 x 128, IP + UDP + payload
packets  [('iface:lo',   4000.0)]      the frames themselves
frames   4000  wire 568000             4000 x 142, the same plus the link header
```

The 56 000-byte gap is `4000 x 14`: loopback's link header, which the device
counters do not include and the capture does. It is a constant, the scenario
knows it, and a run where the gap is anything else is a finding rather than
noise. The check asserts all four numbers exactly.

### Did this link lose anything, and was it a loss or a fault?

`examples/net-interface-drops.yml`. This closes a gap `docs/families/net.md`
names: *"There is no ground-truth test that forces a link error or an
accept-queue overflow."*

A veth pair whose far end is never brought up drops everything handed to it, and
a static neighbour entry keeps address resolution out of the way, so the count
of drops is the count of datagrams sent — exactly, because nothing else is on
that link:

```
{'errors': 0, 'rx_errors': 0, 'tx_errors': 0, 'drops': 1000,
 'rx_dropped': 0, 'tx_dropped': 1000, 'fifo': 0, 'collisions': 0, 'carrier': 0}
```

A thousand sent, a thousand dropped, zero errors. The second half is the
family's own rule and it is checked too: a bad frame and a full queue are
different failures, nothing here was malformed, and a drop counted as an error
would send a reader looking for a cable.

The same scenario found that the aggregating emits of this source answer `0
count/s` for that link. That is under **Defects found** 5, and it is half fixed:
the rate still reports errors alone, and the run now names the thousand frames
it is leaving out of it.

**What this does not reach is a drop inside a queueing discipline.** The obvious
way to make an interface lose packets is `tc qdisc add dev lo root netem loss
50%`, and it moves nothing here: netem frees the frame and reports success
upwards, so the loss lives in the qdisc's own statistics and `/proc/net/dev`
never hears about it. Measured, not assumed — two thousand datagrams sent
through a 50 % netem, `/proc/net/dev` reporting 1043 packets and zero drops.
`net.qdisc` is the source for that loss and it has no collector, so **for this
family, a qdisc drop is invisible and `net.iface` reporting no drops does not
mean no packets were lost on the path.**

### Is this machine refusing connections it was offered?

`net.backlog`, through `examples/net-counters.yml` and `examples/net-snapshot.yml`.

A listener one deep and far more connections than it will ever accept is the
situation those counters exist to name. `count` finds it — nineteen drops in a
second — and `snapshot` used to report zero while the same queue was in the same
state, because a counter source reports an increment and a snapshot's window was
microseconds wide. It now reports the counters as they stand. Both halves are
checked by one scenario against one workload:
`a_snapshot_of_the_accept_queue_reports_the_queue_the_kernel_holds` requires the
totals to contain the drops `count` measured and to be far larger than any
increment over the snapshot's own window could be. **Defects found** 3 has the
entry.

### Did the capture see everything?

`examples/net-capture-accounting.yml`. **The class that justifies the project.**

A capture that loses three percent and says nothing produces wrong conclusions
that look right, and the run is gone by the time anyone notices. So drive it
past what its one-mebibyte ring can hold and check that the arithmetic closes.
Twenty thousand datagrams over loopback are forty thousand frames:

```
events_seen 20302  dropped 19698      20302 + 19698 = 40000
events_seen 20598  dropped 19402      20598 + 19402 = 40000
events_seen 21937  dropped 18063      21937 + 18063 = 40000
events_seen 23416  dropped 16584      23416 + 16584 = 40000
```

Every frame is accounted for. How the two split moves from run to run, because
it depends on how the drain races the sender; the total does not, and the total
is the claim. Twenty thousand and not fewer: at five thousand the drain
sometimes keeps up and the run loses nothing, which is a fine outcome for a
capture and a useless one for a scenario about losing. The kernel counts the drops itself through `PACKET_STATISTICS`,
and the run names both states the ring went through — running out of blocks,
which is the moment before a loss, and being full, which is the loss.

### Is this series spaced the way it claims?

`examples/net-late-ticks.yml`. A series is a claim about spacing as much as
about values.

Ten microseconds is short enough that no machine can read a procfs file that
fast, which is what makes the scenario the same everywhere and independent of
the machine it runs on. Essentially every tick is late, and the run says so:

```
  1247 events over 50.05ms
  LOSS: 0 dropped, 0 failed reads, 1246 late ticks
    1x a round of reads took 108202ns, longer than the 10000ns tick
```

Three things are checked: the count is right, every reason names a duration
longer than the tick it is compared against, and nothing else is claimed — a
late tick lost no event, the read happened and was simply not on time, and
saying otherwise would send a reader looking for data that is all there.

The five hundred near-identical lines that follow are under **Defects found**.

### What happens when the thing being watched goes away?

`net.bytes --iface veth0`, with the interface removed six hundred milliseconds
into a two-second run. Two wrong answers are available — carry on reporting the
last value, or stop with a message about a file — and the tool takes neither:

```
stopped_because: interface `veth0` is not in /proc/net/dev
events_seen 5   observed 600ms of the 2s asked for
```

Both halves matter and both are checked. A series three fifths as long as it was
asked for is otherwise indistinguishable from one that was quiet.

### Which connection is this?

`net.tcp.rtt` with a filter. The ground truth is the port: the kernel handed it
out, the scenario knows it, and no other socket on the machine has it, so
`dport == <port>` has to select exactly one connection and every event it keeps
has to carry that port. It does.

The address does not work the same way, and that is a finding rather than a
lesson — see **Defects found**.

### How long did that connection live, and what crossed it?

`examples/net-connection-life.yml`. **The first source in this family that
measures a connection rather than a link.**

Three numbers are chosen in the workload and nowhere else: the connection is
held open for 300 ms, the client writes exactly 4096 bytes, and the client's
local port is read off the socket. Both ends of a loopback connection are
separate sockets and both close, so one workload produces two events:

```
sport=54278  duration_ns=300232759  bytes_sent=4096  bytes_received=1     inode=2019125
sport=38863  duration_ns=300234569  bytes_sent=0     bytes_received=4097  (no inode)
```

Everything in those two lines is checked. `bytes_sent` is exact, because the
kernel's `bytes_sent` counts payload: not segments, not wire bytes, not
retransmissions. The extra byte on the receiving side is the FIN -- a close
occupies a sequence number, and `bytes_received` is the kernel's reading of the
sequence space rather than a count of payload the application saw. The client's
inode is the one `/proc/<pid>/fd` shows as `socket:[2019125]`, which is the
strongest identity check available without a cookie.

**Two facts about identity came out of writing this, and both are on the
events.**

A socket loses its inode before it reaches `TCP_CLOSE`: `tcp_close` calls
`sock_orphan` first, which unhooks the socket from the file that gave it one. So
the identity read at the close is zero and the identity read at the
establishment is not, and the program keeps both. A socket taken off a listen
queue has the opposite problem -- it has no file until `accept()` gives it one,
which is after it is established -- which is why the second line above has no
inode at all, and why the run says how many of its connections are in that
state.

A socket cookie is generated **lazily**, the first time anything asks the kernel
for one. A connection that opens and closes with nothing looking at it has none,
so it carries no `sock` key and can be joined to nothing. `net.tcp.rtt` is in
this session for that reason and not for the round trip: it polls `sock_diag`,
and a `sock_diag` dump is exactly the thing that asks. The two sources together
produce what neither produces alone -- connections named by the same number on
both sides of the kernel boundary, which is what the family's key discipline was
always for.

### Why did the machine throw that packet away?

`examples/net-closed-port.yml`. **The source that makes this family's headline
honest.**

Five hundred and twelve datagrams to a port nothing is listening on. Two
independent parties count them: this scenario, which chose the number, and the
kernel's own `Udp: NoPorts` in `/proc/net/snmp`. Both have to move by at least
512, and the drops have to carry the reason a closed port gives --
`SKB_DROP_REASON_NO_SOCKET`, which is 3.

The contrast is the point of landing the source at all. `net.iface` watches the
same window and reports nothing: the frames arrived perfectly and the stack
discarded them afterwards, so a reader with only the device counters concludes
the machine lost nothing.

**The reason is a number and not a name**, and that is deliberate. `enum
skb_drop_reason` has a hundred and twenty-eight members whose names are in the
kernel's own BTF, which this build reads field offsets out of and cannot yet
read an enum out of. A table of names written into the tool would be a constant
that no run could check against the kernel it is running on, and a mislabelled
drop reason sends a reader to fix the wrong thing. Finding 2 below.

**`protocol` says `no-socket` on almost every drop**, and that is the honest
answer rather than a gap. A packet's own protocol is `skb->protocol`, inside an
anonymous union of `sk_buff`; what this build can reach is the protocol of the
*socket* the drop belonged to, and a packet dropped because nothing was
listening has no socket by construction. So the commonest drop on any machine is
exactly the one whose protocol cannot be recovered. Finding 1.

### Where did the cpu go while nothing was running?

`examples/net-softirq-cost.yml`. **The only source here that measures work
belonging to no process.**

A received packet is processed in softirq context, so its cost is absent from
every per-process number this tool produces. There is no way to inject a number
of `NET_RX` handlers -- how many a transfer costs depends on the driver's
coalescing -- so the check is against the only other party that counts them:

```
degraded: the kernel's own /proc/softirqs counted 260 handler runs on these
          vectors over a window containing this run, and this run bucketed 260
```

That comparison is in the run's own output, not only in the test, because a
reader holding a softirq number wants to know whether it is all of them. The
relationship is exact in one direction and that is the direction that matters:
the window the run observed is inside the one the two file reads bound, so it
cannot have seen more handlers than happened. A run that reports more is
double-counting, and this is the only thing that would catch it.

**Which number is `NET_RX` is read off the running kernel.** The vector a
`softirq_entry` tracepoint reports is an index into the kernel's own
`softirq_to_name` table, and `/proc/softirqs` prints that table, in that order,
as its row labels. Nothing in this build assumes `NET_RX` is 3 -- a number baked
in at the wrong value would measure the timer softirq and report it as network
time.

### How long did that packet wait to leave?

`examples/net-queue-delay.yml`. **The strongest statement this family makes
about a number it did not choose, after the injected round trip.**

A queueing discipline that holds every packet for fifty milliseconds, on a
device that exists only inside a network namespace, and two hundred packets
pushed through it:

```
  hist (log2)
    iface:dummy0  n=200  min 33.55ms  mean 33.55ms  max 33.55ms
      [33554432ns, 67108864ns)            200  |################################|

  raw
    net.qdisc  iface:dummy0  wait_ns=33554432 count=200 weight=200
               total_ns=10000359457 bucket=26 iface=dummy0 handle=2156527616 qdisc=808a:
```

`10000359457 / 200` is 50.0018 ms. The bucket cannot say that -- a log2 bucket
is a factor of two wide, and `[33.55ms, 67.11ms)` contains the answer without
being it -- which is exactly why `total_ns` travels beside the bucket in every
kernel-built histogram in this repository.

**`omn` runs outside the namespace and the workload inside it**, which is the
opposite of the four older scenarios in this file and is not a convenience. A
raw tracepoint is not namespaced, so a program loaded on the machine sees
packets queued on a device this process cannot even name; and `bpf()` checks
`CAP_BPF` against the *initial* user namespace, so a program cannot be loaded
from inside an unprivileged one at all. Measured, not assumed:
`sched.runqlat --system` inside `unshare --user --map-root-user --net` answers
"no viable backend", naming both capabilities.

That is also why each row carries the interface's name as the *kernel* read it.
An interface index is only meaningful inside the namespace that assigned it, so
resolving `dummy0` here, afterwards, would find someone else's device or
nothing.

**Loopback has no queueing discipline**, on any machine: `lo` is `noqueue`, the
transmit path calls the driver directly, and neither tracepoint fires. A machine
whose only live link is loopback measures nothing here, correctly, and the run
says so in its own output rather than leaving an empty histogram to be read as a
broken collector.

### Did this path lose anything, and did the stack notice?

`examples/net-retransmissions.yml`.

A queueing discipline is added *after* the handshake, so an established
connection finds its path gone. What is checked is not the number of resends,
which is an accident of `TCP_RTO_MIN` and exponential backoff, but the agreement
between two counts of the same events: this run's, and `Tcp: RetransSegs` in
that namespace's own `/proc/net/snmp`. The namespace is what makes the kernel's
number exact -- the only connection on that network is the scenario's -- and the
workload prints its local port so the comparison is over that one socket.

Three resends, three counted, and every one of them from `ESTABLISHED`. The
state is on every event with its name beside it, because three retransmissions
from `SYN_SENT` are a handshake nobody answered and one from `FIN_WAIT1` is a
close that did not land: three different faults with the same number.

### What happens when the machine loses packets faster than this can read them?

`examples/net-drop-storm.yml`. **The class that justifies the project, for the
second source in this family that can lose.**

Six senders and a quarter of a million datagrams to a closed port. The BPF ring
buffer does not report its own overflows -- when a program cannot reserve space
the helper returns an error, the event is gone, and nothing anywhere records
that it existed -- so every program in this layer carries a per-cpu counter
beside its ring, and this is the check that the counter reaches the user:

```
  LOSS: 54038 dropped, 0 failed reads, 10 late ticks, 0 undecoded, 0 throttled
    10x a round of reads outran the 10.0ms tick: worst 12.0ms, 11.2ms on average
    40x the drop ring was full: the machine lost packets faster than they were
       drained, so this run's own count of them is short
```

Two things in that output are the reason the scenario exists. The count of drops
is a **lower bound** and the run says which way it is wrong. And the late ticks
are counted apart from the dropped events, because they are different facts --
one is a number of events that were lost, the other a number of reads that
started late -- and a run that folded them together would claim a precision it
does not have.

### Is the network the problem, or is it us?

`examples/net-health.yml`, inherited from before this wave and left as it was.
It is the only session here written to be *run* rather than checked: three cheap
always-on sources over thirty seconds, answering three different failure modes,
none of them needing a privilege. Nothing checks its numbers, because there is
no workload under it to check them against — it is a thing to point at a machine
that is misbehaving, and it is honest about what it cannot tell you, which is
whether a slow request was slow *because* of any of it. The scenarios above are
the same three sources with a workload underneath.

### Does every axis the catalog declares work?

Four legs, because a session observes one target over one window and the modes
cannot share one:

| Leg | Session | Sources | Covers |
|---|---|---|---|
| poll | `net-capability-sweep.yml` | six | `raw`, `timeline`, `summary` |
| count | `net-counters.yml` | four | `rate`, `raw`, `summary` |
| snapshot | `net-snapshot.yml` | three | `raw`, `summary` |
| trace, netlink | `net-injected-rtt.yml`, and a check of its own | `net.tcp.rtt`, `net.hwts` | `raw`, `hist`, `timeline`, `summary` |
| trace, eBPF | `net-bpf-sweep.yml` | the five raw-tracepoint sources | `hist`, `topk`, `rate`, `raw`, `timeline`, `summary` |

The eBPF leg is five sources over one window with four mebibytes crossing
loopback under it, and it checks the same thing the others do: every request
produced exactly the emits it asked for, and each was served by `bpf:raw_tp`
rather than falling back. Two of the five have to have seen something -- a
connection opened and closed, and the receive softirq that carried it -- and the
other three are a machine's own business: a quiet link retransmits nothing and
drops nothing, and loopback has no queueing discipline at all.

**Three pairs the entries imply are refused, and correctly.** A catalog entry
lists its modes and its emits as two flat lists, so it promises their cross
product, and the mode x emit matrix is narrower:

```
net.tcp.retransmit --mode count --emit topk      exit 2
net.drops          --mode count --emit topk      exit 2
net.softirq        --mode count --emit hist      exit 2
```

A `count` window produces one measurement per series, and neither a ranking over
it nor a histogram of it means anything. The set is pinned by
`the_pairs_these_entries_imply_and_the_matrix_refuses_are_the_three_recorded`,
so a fourth cannot arrive quietly -- and the day `count` can rank what it
counted, which is a request in `docs/open-findings.md`, two of them go away.

The count leg carries the ground truth: four mebibytes are pushed through
loopback while it runs, and the `lo` series has to have seen them. The poll and
snapshot legs are structural — every request produced exactly the emits it asked
for, and each source's numbers are checked in its own scenario above. That is
the honest division, and it is stated here rather than dressed up.

The format axis is a flag rather than a session. A `net` result goes out as
`json`, `yaml`, `toml`, `ndjson`, `chrome` and `speedscope`, and the chrome trace
has to carry the points rather than merely be produced. `folded` and `pprof`
carry stacks, `net` has none and never will — a frame off the wire has no call
stack — and both refuse a rate by name.

---

## Workloads

Phase two reuses these. Each is in `tests/scenario_net.rs` and each guarantees a
property chosen in its source.

| Workload | What it guarantees |
|---|---|
| `push_through_loopback(n)` | exactly `n` bytes over a TCP connection on `lo`, both ends drained before it returns |
| `flood_loopback(port, n, size)` | exactly `n` UDP datagrams of `size` bytes to a *bound* port, so they are delivered rather than answered with an ICMP unreachable |
| `listener_with_backlog(1)` | a listening socket one deep, made by hand because `TcpListener::bind` asks for 128 |
| the veth pair with a down peer | every frame handed to it is dropped at the device, and counted |
| `netem delay 25ms` on `lo` | a round trip of exactly 50 ms for every connection over loopback |
| the namespace helper | a network where the only traffic is the scenario's, which is what makes the counts exact rather than lower bounds |
| `workload_in_a_namespace(script)` | the same network, with the *workload* inside it and `omn` outside: a raw tracepoint is not namespaced, and `bpf()` refuses to load from inside an unprivileged user namespace |
| `netem delay 50ms` on a dummy device | a queueing delay of exactly 50 ms for every packet, on a link nothing else uses |
| `netem loss 100%` added after the handshake | an established connection whose path is gone, so every segment is retransmitted |
| 512 datagrams to a closed port | exactly that many drops with one reason, and the same movement in `Udp: NoPorts` |
| six senders, 240 000 datagrams | more drops than a 512 KiB ring drained every 10 ms can hold |

**The last one is the reusable part.** `rerun_inside_a_namespace` re-runs a test
inside `unshare --user --map-root-user --net`, where it holds `CAP_NET_RAW` and
`CAP_NET_ADMIN` and has a loopback nobody else is using. Any family that wants
an exact count of anything network-shaped wants this.

`netem delay 25ms` is also the confounder P4 asks for: a run whose latency was
injected is not comparable to one whose latency was the path.

### What `net` contributes to a crossing

The socket. `sock:cookie` is the key that survives softirq processing, where a
pid does not — on receive, the work happens outside any process context — and
`net.tcp.rtt` already keys its series on it. Every unbuilt source in this family
carries the same key, so when they land they join to it directly.

Every session here that can carry a key carries one: `iface` on the volume and
link sources, `sock` on the round trip. `net.backlog` carries none, and that is
correct: the accept queue belongs to the machine, and the emit layer labels that
series `system`.

---

## What cannot be verified here, and why

**Per-process network volume.** `/proc/net/dev` has no idea which process moved
a byte, and the honest outcome is the one the tool takes: name the backend that
would answer rather than divide a machine-wide number. Checked as a refusal, not
as a measurement.

**A real round trip, as opposed to the kernel's estimate.** `net.tcp.rtt`
reports what the stack believes. Splitting a measured latency into NIC, kernel
and application segments is `net.hwts`'s per-packet backend, which is not built.

**A qdisc drop.** Netem's loss does not reach `/proc/net/dev`, so `net.iface`
cannot see a frame dropped inside a queueing discipline; `net.qdisc` measures
how long a packet *waited* there and not whether it came out. What does see it
is `net.drops`, through `qdisc_drop` reaching `kfree_skb` with its own reason --
and that is a claim this family has not checked, because the check wants the
namespaced workload of `net-queue-delay.yml` and the drop accounting of
`net-closed-port.yml` in one session, which is a scenario nobody has written.

**A driver-specific counter.** `net.iface` reports the counters every driver
keeps. The long list `ethtool -S` prints needs the ethtool generic-netlink
family resolved through `CTRL_CMD_GETFAMILY`, and the requirement is reported
unverified rather than answered as `NETLINK_ROUTE`, which would be a different
question's answer.

**`net.hwts` on a machine with no PHC.** The development machine has one, on
`enp38s0`, and the ground truth there is the passage of time: two independent
oscillators polled over a known window may drift, because a free-running clock
is allowed to, but cannot move by anything like the window itself. A machine
whose interfaces keep no clock, or keep one this user may not read, says so and
stops — out of reach and absent are different answers and only the first is a
privilege to grant.

**A connection's peer, a dropped packet's protocol, and a drop reason's name.**
Findings 9 and 10. None of the three is a measurement this family declined to
make; all three are fields the shared BTF reader cannot reach, and each run says
which ones it could not carry rather than carrying a zero.

**`net.qdisc` on a machine whose only live link is loopback.** `lo` is
`noqueue`, so no packet on it ever enters a queueing discipline. The scenario
builds a device with one inside a namespace; a machine that allows no
unprivileged user namespace has nothing here to measure a queue on, and the test
says so and stands down.

**Anything needing `CAP_BPF` and `CAP_PERFMON`.** Six scenarios need them, and a
machine that does not grant them cannot exercise any of the five eBPF sources.
They are file capabilities and every `cargo build` drops them, which is why
`make` exists; a green suite on a binary without them is not evidence about any
of this.

**Anything needing `CAP_NET_RAW` on a machine that forbids user namespaces.**
Four scenarios build a network of their own: the two-mechanism comparison, the
capture accounting, the injected round trip, and the vanishing interface. On a
machine with no `unshare`, no unprivileged user namespace, no `ip`, or no
`netem`, each of them names on stderr exactly which of those is missing and
stops. They are still in `examples/` and still in this document, so a machine
that can serve them runs them. Use `cargo test -- --nocapture` to see which
stood down; `libtest` has no way to report a skip and this repository's
convention is to say it on stderr.

**A session that exists to be refused.** `tests/cli.rs` holds the invariant that
every file in `examples/` validates, so the rejections session cannot ship
there. It is written by `tests/scenario_net.rs` into the target directory
instead, where the test validates it and counts the refusals. Here is its text
for a reader who wants to run it — four requests, five refusals, because the
second one is wrong on two axes at once:

```yaml
name: four requests that cannot mean anything
duration: 1s
format: text
requests:
  - {label: queue-per-link, source: net.backlog, target: {iface: lo}, mode: count, emit: [rate]}
  - {label: sampled-counter, source: net.bytes, target: {iface: lo}, mode: sample@997hz, emit: [hist], keys: [iface]}
  - {label: summarised-capture, source: net.capture, target: {iface: lo}, mode: trace, emit: [summary], keys: [iface]}
  - {label: counted-histogram, source: net.bytes, target: {iface: lo}, mode: count, emit: [hist], keys: [iface]}
```

---

## The three refusals, and whether the reason is the right reason

| It says | It means | Wanted | `net` gets |
|---|---|---|---|
| a rejection | the request cannot mean anything | 2 | **2** |
| no viable backend | this machine cannot serve it | 1 | **2** |
| no collector | this build does not implement it | 3 | **3**, except one |

The eBPF sources give all three, and
`the_three_answers_an_ebpf_net_request_can_be_refused_with` asks for one of
each: a sampled connection lifetime is a request that cannot mean anything (2),
a `net.tcp.lifetime --pid` is a target the catalog offers and this build cannot
serve (3), and a forced `bpf:tp` is a road this machine has closed (1). The
fourth answer in that test is the one worth keeping apart from the second:
`net.drops --pid` is also 2, because the catalog never offered a process target
for it at all, and only a target it *did* offer is a gap anybody can close.

The messages are good. Every rejection names the axis that is wrong and lists
what the source does accept; `net.capture` names `CAP_NET_RAW` and says in the
same breath that the request is valid; `net.hwts --iface lo` says where
loopback's timestamps do come from, which is the part that stops a reader
hunting for a flag; `net.hwts --pid 1` names the backend that is missing *and*
why the one that exists was passed over.

Two of the codes do not match the messages. Both are below.

---

## Defects found

Thirteen, in the order a reader is likely to meet them. Each is pinned by an
assertion in `tests/scenario_net.rs`, so that fixing one is noticed here rather
than silently changing what a caller sees. The last five arrived with the eBPF
sources of the sixth wave and four of them are the same defect wearing four
hats.

Two carry a **fixed** marker, with the reproduction left in place: the session
and the output that found them are what says a regression has happened.
`docs/fixes/net.md` has what was done and what it cost.

### 1. `net.bytes` declares an emit no mode it declares can produce

**Fixed** by stream B of the fifth wave; see `docs/fixes/catalog.md`. `topk` was
taken out of the entry, so nothing promised it and the refusal named the source.
**Served** since the thirteenth wave: `count` can rank a counted window
(stream R's `docs/fixes/budget.md`), so `topk` is back in
the entry and `--mode count --emit topk` answers. The reproduction below is
what it said before either.

It was removed rather than served because the decision that would serve it is
not the catalog's: `count` mode refusing to rank what it counted is one arm of
`ModeKind::valid_emits`, written up under "Needs from another stream" in
`docs/open-findings.md`. The day it changes, `topk` goes back into the entry.

The entry lists `topk`, `omn list net.bytes` prints it, and every request that
could ask for it is refused:

```console
$ omn net.bytes --iface lo --mode count --duration 300ms --emit topk
error: [net.bytes #1] mode `count` cannot produce `topk`
  hint: in `count` mode the valid emits are rate, summary, raw
$ omn net.bytes --iface lo --mode poll@100ms --duration 300ms --emit topk
error: [net.bytes #1] mode `poll` cannot produce `topk`
  hint: in `poll` mode the valid emits are raw, timeline, summary
```

`topk` comes from `sample` or `trace`, and `net.bytes` declares neither. The
ranking itself is expressible — the collector emits one event per interface, so
the series exist — which makes this the one case where the emit is wanted, the
data is there, and the matrix is in the way. Either the emit does not belong in
the entry or `count` should be able to rank what it counted; the two are
different decisions and neither is this stream's.

`every_emit_a_net_source_declares_is_reachable_from_a_mode_it_declares` walks the
whole family and pinned this as the only one; it now requires the set to be
empty. `src/validate.rs::check_catalog` applies the same rule to every family.

### 2. A loss that repeats is printed once per occurrence

> **Fixed** (stream A) in `src/collect/mod.rs`, for every family at once. The
> mechanism was never wrong, as this entry says; the note was, and the duration
> has moved out of the key into `losses.overrun`. Asserted by
> `a_poll_faster_than_its_own_read_counts_every_late_tick`.

`Losses::reasons` is a map from reason to count, and the rendering prints `Nx`
before each — which exists to collapse a repetition. The late-tick note defeats
it by putting the measured duration in the key:

```console
$ omn -f examples/net-late-ticks.yml | wc -l
522
$ omn -f examples/net-late-ticks.yml
  1247 events over 50.05ms
  LOSS: 0 dropped, 0 failed reads, 1246 late ticks
    1x a round of reads took 108202ns, longer than the 10000ns tick
    1x a round of reads took 33341ns, longer than the 10000ns tick
    1x a round of reads took 33381ns, longer than the 10000ns tick
    ... 512 more, one per distinct nanosecond count
```

Five hundred lines of output for one reason, from a run of fifty milliseconds,
and nothing bounds it: the list grows with the loss, which is exactly when it is
least readable. A run of an hour at this cadence prints millions. The capture's drops, whose note carries no number,
collapse correctly to `35x` — so the mechanism works and it is the note that is
wrong.

### 3. `snapshot` on a counter source can only ever answer zero — **fixed**

*Fixed by stream F. A snapshot now reports the counters as they stand, under
`*_since_boot` names and with a `basis` attribute saying what the number is, and
takes no closing read: there is no window to close. The reproduction below is
what the tool did before, and*
`a_snapshot_of_the_accept_queue_reports_the_queue_the_kernel_holds` *now
requires the answer to contain the drops `count` measured and to be far larger
than any increment over the snapshot's own microsecond window could be. The
shape is `mem.swap`'s, which had solved the same problem for the same reason.*

A counter source reports the increment between two reads. A snapshot's two reads
are its one read and the closing read that follows it, about a hundred
microseconds apart. So `net.iface --mode snapshot` and `net.backlog --mode
snapshot` report an increment over a hundred microseconds, which on anything
short of a saturated machine is zero.

It is not an idle machine reporting correctly. With a one-deep accept queue being
hammered continuously, the same source in the two modes, seconds apart:

```console
$ omn net.backlog --system --mode count --duration 1s --emit summary --emit raw
  summary
    system n=2     min 1 count  mean 10 count  max 19 count  last 1 count
  raw
    75362631494573  net.backlog    drops=19 overflows=19 syncookies_sent=0 syncookies_failed=0

$ omn net.backlog --system --mode snapshot --emit summary --emit raw
  summary
    system n=1     min 0 count  mean 0 count  max 0 count  last 0 count
  raw
    75362654838011  net.backlog    drops=0 overflows=0 syncookies_sent=0 syncookies_failed=0
```

A reader taking a snapshot to ask *is this machine refusing connections right
now* is told no. On a loaded machine the hundred-microsecond window occasionally
catches one drop, which is the same point rather than a different one: the
number answers about that window and about nothing else. The catalog's caveat
explains the mechanism — these are counters, and the first read is a baseline —
but the mode is still offered and still answers with a number that carries no
information. The totals since boot, which is what a snapshot of a counter would
plausibly mean, are not reachable through any mode.

Stream B of the fifth wave left the mode in the catalog and wrote the contract
up for whoever owns the collector, under "Needs from another stream" in
`docs/open-findings.md`: on `snapshot`, report the absolute counter under its
own name rather than a delta over a window the mode does not have. That is what
`mem.swap --mode snapshot` already answers with, and what `io.pressure`'s caveat
states as the rule. Dropping the mode instead would break
`examples/net-snapshot.yml` and would refuse a question worth answering.

### 4. A `snapshot` request beside a polled one is silently polled

> **Fixed** (stream A). A snapshot is a property of the request, not of the
> session that happens to hold it: each request is driven by its own `Pace`.
> Asserted by `a_snapshot_beside_a_poll_is_still_a_single_read`.

The snapshot flag is a property of the session, not of the request: the runner
sets it only when *every* request is a snapshot. One that is not gets read on
every tick of the shared window, and nothing says so.

```console
$ omn -f a-snapshot-beside-a-poll.yml --format json
polled         events_seen  35  stopped_because the requested duration elapsed
snapshotted    events_seen   5  stopped_because the requested duration elapsed
```

The second request asked for one read and took five. The output is not a lie —
`stopped_because` says what happened — but the request's own mode was
reinterpreted without a word, and the run's notes are where a degradation of
this kind belongs.

### 5. Every aggregating emit of `net.iface` answers about errors alone — **half fixed**

*Stream F could not split the series: a series is keyed on the first key of its
events, and the key model has no variant for which counter a number is, so one
event per counter kind would collapse back into one summed series — the thing
the catalog's caveat forbids. The signature for the shared-layer change is under
"Needs from another stream" in* `docs/open-findings.md`. *What the collector now
does is stop the omission being silent: a run where an interface lost frames to
a counter the series does not carry says so as a degradation, which every emit
and every format that carries a note carries. The rate is still zero, and it is
no longer quiet about it.*

The collector's measured value is `errors`, deliberately: a bad frame and a full
queue are different failures and summing them would hide which happened. The
consequence is that `rate`, `timeline` and `summary` — everything but `raw` —
answer about errors, on a source whose own summary line is *"Interface errors,
drops and pause frames"*.

For the link that had just dropped exactly a thousand frames:

```
rate [('iface:veth0', 0.0)]
raw  {'errors': 0, ..., 'drops': 1000, 'tx_dropped': 1000, ...}
```

`0 count/s`, for a link losing everything given to it. Keeping the two apart is
right; the way it is done makes one of them unreachable from every emit that
aggregates. One event per counter kind, or a key that tells them apart, would
keep the separation and make both visible — but that is a shape decision for
whoever owns the collector.

Stream B of the fifth wave found this to be a collector defect rather than a
catalog one and left it there, with the request written up in
`docs/open-findings.md`. `catalog/net.yaml` now carries the caveat that says
what the aggregating emits answer about and where the other counters are, so
that the `0 count/s` reaches a reader with its explanation attached.

### 6. `daddr ==` can never match

`daddr` is declared as a filter field on `net.tcp.rtt`, and the attribute it
compares against carries the address *and* the port:

```
{"name": "daddr", "value": "35.190.46.17:443"}
```

So an equality against an address matches nothing, forever, and the run reports
it as an ordinary empty result:

```console
$ omn net.tcp.rtt --system --mode poll@100ms --duration 250ms --emit summary \
      --filter 'daddr == "127.0.0.1"'
  filter `daddr == '127.0.0.1'` kept 0 of 608 events, evaluated after reading

$ omn net.tcp.rtt --system --mode poll@100ms --duration 250ms --emit summary \
      --filter 'daddr =~ "127.0.0.1"'
  filter `daddr =~ '127.0.0.1'` kept 8 of 608 events, evaluated after reading
```

`=~` works, so the field is usable once you know. Nothing tells you: `dport` and
`sport` are already separate numeric attributes, so the port inside `daddr` buys
nothing and costs the obvious filter. Either the attribute should be the address
alone or the caveat should say what it holds.

### 7. `net.hwts --mode trace` says "not built" and exits 1

> **Fixed**, and then fixed again. Today a `trace` of an interface never
> reaches a collector: `netlink:ethtool` declares `modes: [poll, snapshot]`
> (the fifteenth wave), so no backend of the chain declares `trace` per
> `iface`, and the validator refuses the pair before resolution, exit 2,
> naming what each backend serves:
>
> ```console
> $ omn net.hwts --iface lo --mode trace --duration 200ms --emit raw
> error: [net.hwts #1] `net.hwts` cannot be observed in `trace` mode per iface: no
> backend of its chain declares both, so no machine could serve it (`so_timestamping`
> not per iface, in trace; `netlink:ethtool` per iface, in poll, snapshot)
>   hint: per iface it serves poll, snapshot; `trace` is served per launch, pid, tid
> $ echo $?
> 2
> ```
>
> The same mode of a launched program resolves onto `so_timestamping`, which
> nobody has written: exit 3, *"no collector is written for `net.hwts` on
> backend `so_timestamping` yet"*, as `DECISIONS.md` settled for "valid, and
> not built, wherever it is discovered". Where no interface's PTP clock can be
> read -- the CI's runners keep `/dev/ptp1`, root's alone -- the machine is the
> first answer and it is exit 1, `so_timestamping: phc: missing` in the chain.
> All three are asserted by
> `the_trace_leg_of_the_sweep_answers_or_names_the_backend_it_would_need`,
> which decides which machine it is on by opening the interfaces' clocks
> itself. What follows is the defect as found.

`DECISIONS.md` settled that *"valid, and not built is exit 3, wherever it is
discovered"*, and `UnsupportedTarget` joined `NoCollector` there. A mode is the
same answer and was not moved:

```console
$ omn net.hwts --iface lo --mode trace --duration 200ms --emit raw
error: cannot start collecting `net.hwts`: `trace` on net.hwts means a timestamp
per packet, which is the `so_timestamping` backend and is not built; this backend
reads what an interface can stamp and how far its clock is from ours, so ask for
`--mode snapshot` or `--mode poll`
$ echo $?
1
```

The message says "is not built" and the code says "this run failed". The
collector returns `CollectError::Start`, which `not_built_yet` does not match.
This is the case the brief names — *a mode that resolves and then refuses at
attach time* — and it is the only one in the family.

### 8. "This machine cannot serve it" and "this cannot mean anything" share exit 2

> **Fixed** (stream A). Exit 1. This entry is right that it was a decision rather
> than a bug and that the man page and the brief disagreed; the brief won, and
> `man/omn.1` is the thing that has to move -- the replacement text is in
> `docs/open-findings.md` for the coordinator, who owns it. Asserted by
> `a_machine_that_cannot_serve_a_net_request_names_what_it_is_missing`.

A resolver failure prints a hint saying the request is valid, and then exits with
the code for a request that is not:

```console
$ omn net.capture --iface lo --mode trace --duration 300ms --emit raw
error: `net.capture` has no viable backend on this machine:
  af_packet: af_packet:tpacket_v3: missing: `af_packet:tpacket_v3` needs CAP_NET_RAW,
  which uid 1000 does not have; this is a privilege to grant, not a kernel to change
  hint: the request is valid; it is this machine that cannot serve it.
        Use --no-resolve to write it down for one that can.
$ echo $?
2
```

`main.rs` returns `EXIT_REJECTED` for every `resolve` error, and `man omn.1`
documents exit 2 as both *"it cannot mean what it asks for"* and *"this machine
cannot serve it"*. The brief's table asks for these to be told apart, and the
distinction is the one that matters to a caller that never reads the message:
raising privileges fixes one and nothing fixes the other. Against the man page
the tool is correct, so this is a decision to take rather than a bug to fix —
but the man page and the brief disagree, and one of them should move.

---

## Smaller things, not defects

**`count` observes a whole second whatever duration is asked.** `--mode count
--duration 300ms` reads at zero, sleeps out the one-second cadence, reads again
and stops, having observed 1.00 s. The header reports the window it actually
observed, so nothing is misstated, but `stopped_because: the requested duration
elapsed` after three times the requested duration reads oddly, and a scenario
author budgeting test time will be surprised once.

**The validator answers for every rejection at once; the collect stage answers
for one missing collector at a time.** `examples/net-not-built.yml` holds two
unbuilt sources and reports the first. Fixing one shows you the next.

It held three, and two of them had to go: they needed `bpf:tp`, whose numeric
event id lives in a root-only file, so on this machine they are not an unwritten
collector but a machine that cannot serve them, and the tool answers 1 rather
than 3. A session mixing the two answers demonstrates neither, because
resolution happens before any collector is looked for. Finding 35 of
`docs/open-findings.md` has the class.

**A timeline's closing read shares the previous offset.** The final read that
closes a counter's window lands at the same rendered offset as the tick before
it, so a four-point timeline at 100 ms prints `+0ns +100ms +200ms +200ms`. It is
the right value at the right time; the rendering rounds two instants together.

**For the coordinator: these scenarios move traffic, and some of the suite
measures a quiet machine.** `make check` is clean here, run against this branch
alone. It is not always clean when eight worktrees are running their suites at
once: at a load average around ten, `family_sched`, `family_cpu`, `perf` and
`budget` each failed in ways that are about the machine rather than about any
change — *"a task alone on a cpu waits for nothing, and this one waited 26 % of
the window"*. Those tests failed with this branch's scenarios present and also
without them, so this is a property of running the suites in parallel and not of
this stream; it is recorded because whoever merges eight branches will meet it.
What this stream did about its own share: four mebibytes pushed through loopback
rather than eight, and a connect every other millisecond rather than as fast as
the loop can go. The capture's twenty thousand datagrams stayed, because five
thousand sometimes fails to overflow the ring and a loss scenario that loses
nothing measures nothing. Two seconds of cpu for nineteen scenarios.

### 9. Half of a connection's identity is unreachable, and it is the half a reader asks for

The largest of this wave, and the one the other three below are consequences of.

`Btf::member_offset` resolves a member by name inside a *named* structure. A
great deal of what the network stack keeps is not in one:

| What | Where | Wanted by |
|---|---|---|
| `skc_daddr`, `skc_dport` | an anonymous struct inside an anonymous union of `sock_common` | `net.tcp.lifetime`, `net.tcp.retransmit` -- the catalog declares both as filter fields |
| `skb->protocol`, `skb->skb_iif`, `skb->dev` | an anonymous union of `sk_buff` | `net.drops` -- the catalog declares `protocol` as a filter field |

A real CO-RE relocation walks into anonymous members, which is how
`skb->protocol` resolves in a compiled program. This one cannot, so what the two
TCP sources report is the **local** endpoint of each socket and `net.drops`
reports the protocol of the socket a drop belonged to, which for a
receive-side drop is usually none.

Writing the offsets into the collectors instead was considered and refused: it
is exactly the staleness the whole layer exists to prevent, and the one failure
mode -- a plausible number read out of the wrong field on a kernel nobody
tested -- is the one this project is built not to have. A predicate over a field
no event carries is false everywhere, which empties a series and looks like a
quiet link, so each run says in its own output which fields those are. The
signature that would close it is under **Needs from another stream** in
`docs/open-findings.md`.

### 10. The drop reason has no name

`enum skb_drop_reason` has a hundred and twenty-eight members and the kernel
publishes all of their names in its own BTF, which this build already reads and
already parses. What it has no reader for is an enum.

So `net.drops` reports `reason=3` where it could report
`SKB_DROP_REASON_NO_SOCKET`, which is a real cost for the source whose whole
point is that it says *why*. A table of names in the collector would close it
today and was refused for the same reason as finding 9: the values would be a
constant no run could check against the kernel it is running on, and a
mislabelled drop reason does not read as wrong, it reads as a different fault.

The scenario turns that into a check rather than a caveat: it asserts the
number a closed port gives. A kernel that renumbers the enum fails the test
instead of quietly reporting something else.

### 11. A socket cookie does not exist until something asks for one

The key this family is built on is generated **lazily**: `__sock_gen_cookie`
assigns one the first time anything calls for it, and nothing in the kernel
calls for it on a socket's own account. So a connection that opens and closes
with nothing dumping it has a cookie of zero, which is not a cookie -- keying an
event on it would join every anonymous connection to every other -- and
`net.tcp.lifetime` emits no `sock` key at all for those.

It is not a defect in the collector and it is not fixable there. What it means
is that **a `sock:cookie` join between an eBPF source and `net.tcp.rtt` works
because `net.tcp.rtt` is running**: its `sock_diag` poll is what generates the
cookies. `examples/net-connection-life.yml` is built that way and says so, and
the scenario asserts the cookie is there. A session that asked for
`net.tcp.lifetime` alone and expected to join it to something later would find
nothing, and nothing in the catalog warns of it.

Two more identities were found on the way, both now on the events: a socket
loses its inode before it reaches `TCP_CLOSE`, because `tcp_close` calls
`sock_orphan` first, so the program keeps the one it read at the
establishment; and a socket taken off a listen queue has no inode at all until
`accept()` gives it a file, which is after it is established.

### 12. Five entries offer targets and keys no backend can serve

Consequences of the two above, and each is a promise a reader can ask for:

- `net.tcp.lifetime` and `net.tcp.retransmit` declare `pid`, `cgroup` and
  `container` targets. Both ends of a connection become established in softirq
  context, and a retransmission is issued from a timer, so the task current at
  those moments is whichever one the interrupt landed on. Both refuse by name
  with exit 3, which is the right code -- attributing a connection to a process
  needs a socket-to-process map, which is a thing to build rather than a thing
  the machine lacks.
- `net.qdisc` declares `keys: [sock]`. A row aggregated over packets has no
  socket; what it has is the interface, which is what the events carry.
- `net.qdisc` requires only `raw_tracepoint:qdisc_dequeue`, and a wait needs
  both ends. The collector attaches `qdisc_enqueue` as well and refuses by name
  on a kernel that has only one of them, but the probe would report the source
  available on such a kernel.

All four are catalog edits, which belong to whoever owns `catalog/net.yaml`; the
exact lines are in `docs/open-findings.md`.

### 13. `topk` cannot rank the thing worth ranking

`net.drops` declares `topk`, and what a reader wants ranked is the drop
*reason*: which fault cost this machine the most packets. `topk` ranks series,
and a drop's series is its socket -- which most drops do not have, so the
ranking is over one series called `system`.

This is `docs/open-findings.md`'s existing request for a ranking over an
attribute, met for the second time by a second family. It is recorded here
because it is the difference between `net.drops --emit topk` being the first
thing a reader reaches for and being a line that says `1 series`.

---

## What the comparison asked, the twenty-fourth wave

The comparison against `ip`, `nstat`, `ss`, `tcpdump` and the bpftrace and bcc
tools (`docs/fixes/w22-net.md`, `w22-bpftrace.md`) named what `omn` was
missing. This is what landed, each shown on this machine against something
that is not `omn`.

| Asked | Now | Shown against |
|---|---|---|
| `net.bytes`' polled baseline | read at `start`, before the program is released; the first tick passes over | 4 MiB pushed between the baseline and the first tick are in the second (`dev::what_moves_before_the_first_tick_is_measured_from_the_start`) |
| `dir` on `net.packets` | `--keys iface,dir`, one series per direction, `fs:proc` | `ip -s link`: 1860 / 1860 packets each way over a pingpong |
| `net.iface` and `net.backlog`'s closing read | a poll reads nothing at `stop` (the defect `dev.rs` had), and takes its baseline at `start` | the last point is the runner's last tick, not a zero beside it |
| `missed` and `mcast` | two more `net.iface` kinds, `/missed` from sysfs's `rx_missed_errors`, absent rather than zero where sysfs has none | sysfs, every interface here (at zero only) |
| `tcp_info`'s other fields | `rto_ns`, `min_rtt_ns`, `busy_ns`, both rates, `bytes_acked`, `bytes_received`, `bytes_sent`, `unacked`, `lost`, segment counts, windows, `congestion` | `ss -ti` on one connection, field by field (rates times eight) |
| `net.snmp` | every counter of `/proc/net/snmp`, `netstat` and `snmp6` by `nstat`'s name, `raw` only (no key names a counter yet) | `nstat` over 200 refused connects: `TcpAttemptFails` 200 / 200, `TcpOutRsts` 200 / 200 |
| a socket inventory | `net.sockets`: every TCP and UDP socket, state in `ss`'s words, queues, endpoints, owner | `ss -tuan`: 29 sockets, the same count per protocol and state |
| `direction` in `--filter` | `net.capture` filters on `direction`, `saddr`, `sport` | `tcpdump` on `lo`, `direction != "outgoing"`: 932 / 932, 916 / 916 |
| `net.tcp.lifetime` per launch | `--pid` and `-- PROGRAM`, the connection attributed to the process that connected or closed it first (`attributed: owner`) | a launched `sh` running 10 connections: 20 ends, all `owner`, another process's 20 in the same seconds left out |
| `net.tcp.accept` | new, fexit on `inet_csk_accept`, the listener's queue after the accept | `tcpaccept.bt`: the same pid, comm, ends; queue 5 to 0 of 16 where `tcpaccept` prints the accepted socket's copy, 0 to 5 |
| the stack on `net.drops` | `-e stack` (trace only), and `function`, the symbol `location` is in | `bpftrace`'s `kstack`: the same 21 frames, 50 / 50 |

**`net.tcp.connect`'s 40 % in the backends bench is its trampolines.** The
same `-- sleep 0.2` under three sources: `net.tcp.connect` (six fentry and
fexit links) spent 69.2 ms of cpu inside the window, `net.tcp.accept` (one
fexit link) 11.8 ms, `net.tcp.lifetime` (a raw tracepoint) 2.6 ms. About 11 ms
of cpu a trampoline, paid whatever the run sees; over the bench's `launch`
regime on `compute` that is the 40 %. Where in the run it is paid -- the
attach in `start` or the detach in `stop` -- is reasoned, not shown: the
in-window figure is the detach's, if `start` is before the window as the
runner documents. Moving it means fewer trampolines: a `bpf:raw_tp` backend on
`inet_sock_set_state` (`SYN_SENT` to `ESTABLISHED` or `TCP_CLOSE`), which
would not see a connect that failed before a SYN left, and would say so.

**Two things a reader of `tcpaccept` meets.** Its `BL` is the accepted
socket's copy of the listener's queue, taken when the SYN arrived; `backlog`
here is the listener's queue after the accept. And `lo` hands a packet socket
every frame twice -- `outgoing` and `host` -- which `net.capture` keeps and
libpcap drops; `--filter 'direction != "outgoing"'` is tcpdump's count.

**Not done** (`docs/fixes/w24-net.md`): pcap output, which is an encoder and
the header bytes on each event; a key naming a counter, so `net.snmp` has
series; `net.tcp.lifetime`'s `bpf:tp` twin and `--tid`.

---

## What the twenty-fifth wave added

- **`net.capture` carries its frame for pcap**: `header` (hex of the link,
  IP and transport headers, cut where they end -- the payload test caught a
  first version that carried 54 bytes past them), `realtime_ns`, `linktype`.
- **`net.snmp -k counter`**: one series per counter. Over a `curl` of one
  page, `-m count -e summary --filter 'section == "Tcp"'` read
  `TcpActiveOpens 1`, `TcpOutSegs 6`, `TcpInSegs 4`.
- **`net.sockstat`**: `ss -s`'s transport table from `/proc/net/sockstat`
  and `sockstat6`. Beside `ss -s` the same second: TCP 12 (6 + 6), UDP 4
  (2 + 2), RAW 1 (0 + 1), sockets 285 against `Total: 284` a moment before.
- **`net.bytes`, `net.packets` on `fs:proc`**: a baseline older than one
  poll interval at the first tick is retaken there, as `io.bytes` does (a
  unit test; not reproduced live unprivileged, where startup is short).
- **`net.tcp.lifetime --tid`**: the connections one thread owns (unit test).
- **`net.tcp.accept` follows a launched tree** in the kernel through
  `sched_common::Descendants`, the root a constant; a child the map could not
  hold is a counted loss (the program builds here; attach needs CAP_BPF).
- **`net.tcp.connect` over `bpf:raw_tp`**: one program on
  `inet_sock_set_state`, stamping at `SYN_SENT` in the connecting task and
  closing on `ESTABLISHED` or `CLOSE` (`sk_err`), the fentry road's record so
  the collector is shared. No `failed` outcome, the cookie as it stands. It
  builds against this kernel; the attach and its cost against the fentry
  road's 65 ms detach are a privileged run.

## Reading these scenarios

Three things the checks rely on and a reader should too.

**The backend line.** `via fs:proc` and `via netlink` are two different
mechanisms and two measurements taken through them are not comparable — except
where a scenario makes them so on purpose, which is what
`net-two-mechanisms.yml` is.

**The loss line.** `no loss: every read succeeded and no tick ran late` is the
only reading that means the numbers are complete. This family has four ways to
come up short and they are not all the same thing: a capture ring that fills is
a loss, a poll that runs late is a loss, a target that disappears ends the run,
and a connection that opened and closed between two ticks was never there to
lose — the catalog says so, and calling it a loss would be as wrong as staying
quiet about the other three. The first three are exercised above. The fourth —
a counter that went backwards because its interface was replaced, which the
collector counts as a failed read — is the one path here with no scenario: it
needs an interface to be removed and re-created under the same name inside one
run, and the veth scenario removes one and stops instead.

**The window.** `events over 600.4ms` on a run that asked for two seconds is the
run telling you its target went away. The number of events is not the thing to
read; the window is.

**An unknown owner is not a zero.** Two defects the twenty-fourth wave's
review found, of one kind: `net.sockets --pid` on a process whose descriptors
this user cannot read listed no socket and said "no loss", and
`net.tcp.lifetime --pid` sorted a connection no process was seen acting on
(`at_close`) in or out of the tree by whichever task its close interrupted.
The first is now refused with the reason (and a failed read if it becomes
unreadable mid-run); the second is a counted loss under a process target.
