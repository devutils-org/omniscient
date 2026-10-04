# What omn replaces: the `net` family

Every pair is in `catalog/references/net.yaml` and runs through the shared
runner (`cargo test --test reference pairs_net -- --nocapture`). The net
sources compared are system-wide or per interface, so omn cannot launch the
workload: every pair runs `alongside` -- omn for its `--duration`, and the
tool, wrapped to measure the same window (`tests/reference/workloads/net/`),
started beside it; the runner starts the workload a second later, **the
same run, seen by both**. The two windows open and close tens of
milliseconds apart, so other traffic on a loaded machine in that slack
lands on one side only; the tolerances are sized for that, and the runner's
runs on 2026-10-04 came out exact on every counter.

| Tool and use | omn equivalent | Covered | Same numbers | Missing |
|---|---|---|---|---|
| `ip -s link show dev lo`: bytes received | `omn net.bytes --iface lo -m count --keys iface,dir -e summary` | full | yes, exact (67196396 both; tolerance 3 %) -- pair `ip-link-rx-bytes` | -- |
| `ip -s link show dev lo`: bytes sent | `omn net.bytes --iface lo -m count --keys iface,dir -e summary`, series `dir:tx` | full | yes, exact (67196344 both; tolerance 3 %) -- pair `ip-link-tx-bytes` | -- |
| `ip -s link show dev lo`: packets | `omn net.packets --iface lo -m count -e summary` | in part | yes, exact against rx + tx (7264 both; tolerance 5 %) -- pair `ip-link-packets` | a `dir` key on `net.packets`: it sums both directions, `net.bytes` splits them |
| `ip -s link`: errors, dropped, carrier, collisions | `omn net.iface --iface lo -m count -e summary` (series `lo/errors`, `/drops`, `/fifo`, `/collisions`, `/carrier`) | in part | yes at zero (errors, drops, carrier and collisions, rx + tx, abs 0) -- pair `ip-link-errors`: lo never errs, so a non-zero agreement is not shown | `missed` and `mcast` columns; the `-s -s` breakdown (`length`, `crc`, `frame`, `fifo`) is promised by the `netlink:ethtool` backend and not compared here |
| `nstat -az TcpExtListenOverflows TcpExtListenDrops` | `omn net.backlog --system -m count -e raw` (`overflows`, `drops`) | full | yes, exact (62 and 62; tolerance abs 2) -- pair `nstat-listen-overflows` | -- |
| `nstat -az TcpRetransSegs` | `omn net.tcp.retransmit --system -m count -e summary` | full, and per socket and task beyond it | yes, exact (124 both; tolerance abs 8) -- pair `nstat-retrans-segs` | -- |
| `nstat` / `netstat -s`: every other SNMP and netstat counter (`TcpActiveOpens`, `TcpInSegs`, `TcpOutRsts`, `UdpInErrors`, `UdpRcvbufErrors`, `IpExtInOctets`, ...) | none | no | no pair: nothing to compare against | a generic `/proc/net/snmp` + `/proc/net/netstat` counter source (`net.snmp`), keyed by counter name |
| `ss -ti`: a connection's rtt | `omn net.tcp.rtt --system -m poll -e summary --filter 'sport == P'` | full | yes, within 7 % (omn 1154000 against ss 1233250 ns, and 1202538 against 1162333, on 2026-10-04, both every 200 ms; tolerance 15 %, as the two read the estimate at different instants) -- pair `ss-ti-rtt` | -- |
| `ss -ti`: cwnd, ssthresh, retransmits, congestion state | the same source's `cwnd`, `ssthresh`, `retransmits`, `congestion_state` attributes | in part | no pair: only rtt is compared | `bytes_acked`, `bytes_received`, `delivery_rate`, `pacing_rate`, `busy`, `minrtt`, `rto`, `unacked` from `tcp_info`; the congestion algorithm's name |
| `ss -tan` / `ss -tlnp`: list sockets, their state and owning process | none (`net.tcp.rtt` leaves non-established sockets out by design) | no | no pair | a socket inventory source: every socket with state, queues (`Recv-Q`/`Send-Q`) and owner, in `snapshot` mode |
| `ss -s`: socket totals by protocol and state | none | no | no pair | the same inventory source's summary, or `/proc/net/sockstat` as a source |
| `tcpdump -i lo <filter>`: packets captured | `omn net.capture --iface lo -m trace -e raw --filter 'dport == P'` | in part | the pair `tcpdump-captured`, by hand (the runner skips it: its root check wants `sudo -n` to grant everything): 1781 / 1781 and 1804 / 1804 packets, counting omn's events as tcpdump does. On `lo` omn shows each frame twice, `direction outgoing` and `direction host`, where libpcap drops the outgoing copy: 3562 and 3608 raw events | `--filter` on `direction`, so the loopback copy can be left out by omn rather than by whoever reads its output |
| `tcpdump -w file.pcap`: write a capture for Wireshark | none | no | no pair | a `pcap`/`pcapng` output format for `net.capture`'s raw events (it copies headers only, so a snaplen-like capture) |
| `tcpdump -nn` with payload / protocol decode | none | no | no pair | out of scope by design: `net.capture` copies headers only |
