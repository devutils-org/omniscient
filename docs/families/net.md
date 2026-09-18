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

There is no ground-truth test that forces a link error or an accept-queue
overflow. Both are reachable -- a veth pair with `tc netem loss`, and a listener
that never accepts -- and both are worth writing before this family is called
done.

## Needs from the shared layer

Nothing blocking yet. Ahead:

- `topk` emit, for `net.bytes --emit topk` across interfaces (G5).
- A netlink layer for `sock_diag`, `rtnetlink` and `ethtool`. `net.tcp.rtt`,
  `net.iface` and the netlink backend of `net.bytes` all need it, and so do
  parts of `io`. It is family-local for now; if a second family needs it, it
  should move to `src/netlink.rs`.

### The socket cookie was already the right key

`sock_diag` reports a per-socket cookie, and it is exactly the `sock:cookie` the
catalog already names as the key that survives softirq processing. `net.tcp.rtt`
keys its series on it, so when the eBPF backends arrive they join to these
without a translation layer.

### Only established connections have a round trip

A listening or closing socket reports zero. Emitting that as a measured latency
of zero would be a lie with a number attached, so those sockets are skipped and
the catalog says why.

### What the RTT is, and is not

It is the kernel's smoothed estimate — what the stack currently believes about
the path, which is what drives its own congestion decisions. It is not a
measured packet round trip. `net.hwts` is the source that splits real latency
into NIC, kernel and application segments.
