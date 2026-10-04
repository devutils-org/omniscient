# What omn replaces: the `net` family

Every pair is in `catalog/references/net.yaml` and runs from
`tests/reference/net.rs` (`cargo test --test reference net::`); the shared
runner's `pairs_net` reports each as a `gap:`, since it has no `alongside`
mode yet. The net
sources compared are system-wide or per interface, so omn cannot launch the
workload: it runs for a `--duration`, and the tool, wrapped to measure one
run (`tests/reference/workloads/net/`), runs the workload 1.5 s into it --
**the same run, seen by both**. omn's window is wider than the tool's, so
any other traffic on this loaded machine lands on omn's side only; the
tolerances are sized for that, and the runs on 2026-10-04 came out exact.

| Tool and use | omn equivalent | Covered | Same numbers | Missing |
|---|---|---|---|---|
| `ip -s link show dev lo`: bytes received | `omn net.bytes --iface lo -m count --keys iface,dir -e summary` | full | yes, exact (67196604 both; tolerance 3 %) -- pair `net.bytes / ip -s link [rx_bytes]` | -- |
| `ip -s link show dev lo`: bytes sent | `omn net.bytes --iface lo -m count --keys iface,dir -e summary`, series `dir:tx` | full | yes, exact (67197020 both; tolerance 3 %) -- pair `net.bytes / ip -s link [tx_bytes]` | -- |
| `ip -s link show dev lo`: packets | `omn net.packets --iface lo -m count -e summary` | in part | yes, exact against rx + tx (7452 both; tolerance 5 %) -- pair `net.packets / ip -s link [packets]` | a `dir` key on `net.packets`: it sums both directions, `net.bytes` splits them |
| `ip -s link`: errors, dropped, carrier, collisions | `omn net.iface --iface lo -m count -e summary` (series `lo/errors`, `/drops`, `/fifo`, `/collisions`, `/carrier`) | in part | yes at zero (`[rx_errors]`, abs 0): lo never errs, so a non-zero agreement is not shown | `missed` and `mcast` columns; the `-s -s` breakdown (`length`, `crc`, `frame`, `fifo`) is promised by the `netlink:ethtool` backend and not compared here |
| `nstat -az TcpExtListenOverflows TcpExtListenDrops` | `omn net.backlog --system -m count -e raw` (`overflows`, `drops`) | full | yes, exact (62 and 62; tolerance abs 2) -- pairs `net.backlog / nstat [TcpExtListenOverflows]`, `[TcpExtListenDrops]` | -- |
| `nstat -az TcpRetransSegs` | `omn net.tcp.retransmit --system -m count -e summary` | full, and per socket and task beyond it | yes, exact (124 both; tolerance abs 8) -- pair `net.tcp.retransmit / nstat [TcpRetransSegs]` | -- |
| `nstat` / `netstat -s`: every other SNMP and netstat counter (`TcpActiveOpens`, `TcpInSegs`, `TcpOutRsts`, `UdpInErrors`, `UdpRcvbufErrors`, `IpExtInOctets`, ...) | none | no | no pair: nothing to compare against | a generic `/proc/net/snmp` + `/proc/net/netstat` counter source (`net.snmp`), keyed by counter name |
| `ss -ti`: a connection's rtt | `omn net.tcp.rtt --system -m poll -e summary --filter 'sport == P'` | full | yes, within 0.1 % (1068750 ns both on 2026-10-04; tolerance 15 %, as the two read the estimate at different instants) -- pair `net.tcp.rtt / ss -ti [rtt_ns_mean]` | -- |
| `ss -ti`: cwnd, ssthresh, retransmits, congestion state | the same source's `cwnd`, `ssthresh`, `retransmits`, `congestion_state` attributes | in part | no pair: only rtt is compared | `bytes_acked`, `bytes_received`, `delivery_rate`, `pacing_rate`, `busy`, `minrtt`, `rto`, `unacked` from `tcp_info`; the congestion algorithm's name |
| `ss -tan` / `ss -tlnp`: list sockets, their state and owning process | none (`net.tcp.rtt` leaves non-established sockets out by design) | no | no pair | a socket inventory source: every socket with state, queues (`Recv-Q`/`Send-Q`) and owner, in `snapshot` mode |
| `ss -s`: socket totals by protocol and state | none | no | no pair | the same inventory source's summary, or `/proc/net/sockstat` as a source |
| `tcpdump -i lo <filter>`: packets captured | `omn net.capture --iface lo -m trace -e raw --filter 'dport == P'` | in part | not run here: the pair `net.capture / tcpdump [captured]` skips (tcpdump 4.99.7 is installed, but it needs root through `sudo -n`, which this machine does not grant non-interactively; omn's `af_packet` needs `CAP_NET_RAW`, `make CAPTURE=1`) | the pair run; see `docs/fixes/w22-net.md` for the commands |
| `tcpdump -w file.pcap`: write a capture for Wireshark | none | no | no pair | a `pcap`/`pcapng` output format for `net.capture`'s raw events (it copies headers only, so a snaplen-like capture) |
| `tcpdump -nn` with payload / protocol decode | none | no | no pair | out of scope by design: `net.capture` copies headers only |
