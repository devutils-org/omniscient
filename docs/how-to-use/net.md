# `net`: connections, sockets, volume, drops, and one request's life

Lazy default: `omn net -- ./prog` (`net.socket` histogram; `--cost med` adds
`net.tcp.connect`).

## Which source for which question

| Question | Source | Mode |
|---|---|---|
| How long do socket reads and writes take? | `net.socket` | `trace -e hist` |
| How long does connecting take, and does it fail? | `net.tcp.connect` | `trace -e hist` |
| Which connections did a server accept, with what queue? | `net.tcp.accept` | `trace -e raw` (what `tcpaccept` prints) |
| How long do connections live, how much do they move? | `net.tcp.lifetime` | `trace -e raw` (what `tcplife` prints) |
| What is the path's RTT and congestion state? | `net.tcp.rtt` | `poll@1s` (what `ss -ti` prints) |
| Retransmissions | `net.tcp.retransmit` | `trace -e topk` |
| Dropped packets, and why | `net.drops` | `trace -e topk` (`-e stack` for where) |
| Bytes and packets per interface | `net.bytes`, `net.packets` | `poll@1s -e timeline` |
| Interface errors, drops, pause frames | `net.iface` | `poll` |
| Every TCP/UDP socket now (`ss -tuanp`) | `net.sockets` | `snapshot -e raw` |
| Every protocol counter (`nstat`) | `net.snmp` | `count -k counter -e summary` (one row per counter), `poll -k counter -e rate` |
| Sockets by transport (`ss -s`) | `net.sockstat` | `snapshot -e raw` |
| Accept and SYN queue overflows | `net.backlog` | `count` |
| Transmit queueing in the qdisc | `net.qdisc` | `trace -e hist` |
| Time in network softirq | `net.softirq` | `trace` |
| Raw packets | `net.capture` | `trace` (needs `CAP_NET_RAW`) |
| One request's time by state, and the network on each side | `net.request` | `trace` (the program declares its requests) |

## Examples

```console
omn trace net.socket -m trace -e hist,summary -- ./client
omn trace net.tcp.connect -m trace -e hist -- curl -s http://example.com/ -o /dev/null
omn trace net.tcp.accept -m trace -e raw -- ./server
omn trace net.tcp.lifetime -m trace -e raw --system -d 30s
omn trace net.tcp.rtt -m poll@1s -e timeline --pid 1234 -d 30s
omn trace net.drops -m trace -e topk:10 --system -d 30s
omn trace net.bytes -m poll@1s -e timeline -k iface,dir --system -d 1m
omn trace net.sockets -m snapshot -e raw --system
```

## Targets

- `net.socket`, `net.tcp.connect` take a launch, `--pid`, `--tid` natively.
  `net.tcp.connect` also takes `--cgroup`, compared in the program against the
  task that called `connect`, the cgroup and every cgroup below it;
  `net.socket` refuses a cgroup and says why.
  `net.tcp.accept` keeps the accepts of that one process, not of its children;
  `net.tcp.lifetime` follows the process tree.
- `net.bytes` per process is **eBPF only**; from `/proc/net/dev` it is per
  interface and machine-wide, and the resolver says so rather than dividing a
  machine number.
- `--iface eth0` targets an interface: `net.bytes`, `net.packets`, `net.iface`,
  `net.qdisc`, `net.capture`, `net.xdp`.
- `net.drops`, `net.snmp`, `net.backlog` and `net.softirq` are the machine's.
  The counters are those of the network namespace `omn` runs in.

## Reading the numbers

- A **receive's latency includes the wait for data**: a blocking `recv` that
  waited one second for its peer is one second, and is not a slow network.
- Every handshake ends established, refused or failed (`outcome`), each its own
  latency: a refusal in 30 us is not a fast connection. A handshake near 1 s
  is a lost SYN retransmitted after the initial RTO.
- `net.tcp.rtt` is the kernel's smoothed estimate, and only an established
  connection has one. A poll misses connections that lived between two ticks.
- Loopback traffic is counted twice (received and transmitted), and a packet
  socket on loopback sees every frame twice.

## Backends and cost

- fentry sources (`net.socket`, `net.tcp.connect`, `net.tcp.accept`) are `low`
  per event, but attaching the trampolines costs about 65 ms once per run.
- `net.socket` times receives at `sock_recvmsg`, or, on a kernel that inlined
  it into the syscalls, at each family's own receive function
  (`bpf:fentry:families`, inet, inet6 and unix only), and the run says which.
- `net.capture` is `high` and copies headers only (no payload enters the
  process). Watch its drops: a capture that loses packets under load produces
  wrong conclusions.

## `net.request`

The program declares its requests with two USDT probes, `request__start(id)`
and `request__done(id)`, in the executable. Each request is then split into
on-cpu, runnable, sleeping, blocked, `net_rx` and `net_tx`, summing to its
latency by construction. Linux 6.6 or later. `examples/vertical-*.yml` and
`ROADMAP.md` phase 4 are the walk-through.
