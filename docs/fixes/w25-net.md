# Stream net, the twenty-fifth wave -- partial

Branch `feat/w25-net`. Done sequentially by one agent; stopped after the
first item when the session's usage limit was reached.

## What landed

1. **`net.capture` carries what `--format pcap` writes**: `header` (hex of
   the link-layer, IP and TCP/UDP/ICMP headers, cut at their end so no byte
   of payload is held, which `family_net`'s payload test caught on the first
   try), `realtime_ns` (the kernel's stamp) and `linktype` (Ethernet for
   Ethernet and loopback, `LINKTYPE_RAW` for tunnels; an unnamed link layer
   carries no bytes).

## Shown vs reasoned

- Unit tests for the hex, the link types and the header end;
  `family_net::a_capture_reports_the_whole_frame_and_holds_none_of_its_payload`
  passes. Not shown: `tcpdump -r` of the file (no tcpdump here). Wanted:
  `omn trace net.capture --iface lo -e raw --format pcap -o /tmp/cap -- curl -s localhost`
  then `tcpdump -nr /tmp/cap`.

## Not done (the rest of the stream, unchanged from `BRIEF.md`)

1. `counter` attribute and key on `net.snmp`, and its `rate`/`summary`/`timeline`.
2. A `bpf:raw_tp` road for `net.tcp.connect` on `inet_sock_set_state`.
3. `net.tcp.lifetime --tid`; `net.tcp.accept` following a launched tree.
4. `ss -s`.
5. `net.bytes` on `fs:proc`: a stale first baseline, as `io.bytes` had.

## Tests

`--lib collect::net`, `--test family_net --test encoders`, clippy, fmt.
