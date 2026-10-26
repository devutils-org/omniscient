# Stream net, the twenty-fifth wave

Branches `feat/w25-net` (the capture header, merged first) and
`feat/w25-net2`. Done sequentially by one agent, in the main checkout.

## Outside the owned files, said first

- `docs/CATALOG.md`: the `net.tcp.connect`, `net.tcp.lifetime`, `net.snmp` rows
  and notes, a `net.sockstat` row.

## What landed

1. **Defect: `net.bytes` and `net.packets` on `fs:proc` counted bytes from
   before the window** when their baseline was read during a long startup
   (top, finding 2, the `io.bytes` fix carried over): a baseline older than one
   poll interval is retaken at the first tick.
2. **`net.capture` carries its frame**: `header` (hex, cut at the end of the
   headers, never payload), `realtime_ns`, `linktype` (Ethernet and loopback
   1, tunnels 101; an unnamed link layer carries no bytes).
3. **`net.snmp` keyed on `counter`**, with `summary`, `rate`, `timeline`.
4. **`net.sockstat`**: `ss -s` from `/proc/net/sockstat` and `sockstat6`.
5. **`net.tcp.lifetime --tid`**: one thread's connections.
6. **`net.tcp.accept` follows a launched tree** through
   `sched_common::Descendants` in the program; unfollowed children counted.
7. **`net.tcp.connect` over `bpf:raw_tp`** on `inet_sock_set_state`, second
   in the chain, degraded: no `failed`, the cookie as it stands.

## Shown vs reasoned

- `counter` (shown): a `curl` of one page read `TcpActiveOpens 1`,
  `TcpOutSegs 6`, `TcpInSegs 4`.
- `net.sockstat` (shown): against `ss -s` the same second, TCP 12 (6 IPv4,
  6 IPv6), UDP 4, RAW 1, the same numbers.
- The capture header (unit tests; `family_net`'s payload test passes).
- The stale baseline, `--tid` (unit tests).
- The accept descendants and the connect state road (reasoned): both build
  against this kernel's BTF and are checked by instruction-order tests; the
  attach needs CAP_BPF.

## Privileged runs wanted

- pcap: `omn trace net.capture --iface lo -e raw --format pcap -o /tmp/cap -- curl -s localhost`
  then `tcpdump -nr /tmp/cap` (needs `make CAPTURE=1` and a tcpdump).
- The connect state road against the fentry road, cost and counts:
  `omn trace net.tcp.connect -e summary --backend force:bpf:raw_tp -- curl -s http://example.com`
  and the same with `force:bpf:fentry`; the cost lines say what the detach
  saved, and a refused connect (`curl localhost:1`) reads `refused` with
  errno 111 on both.
- Accept on a launched tree: `omn trace net.tcp.accept -e raw -- sh -c 'python3 -m http.server 8765 & sleep 1; curl -s localhost:8765 >/dev/null; kill %1'`
  keeps the server's accept (the child of `sh`).

**Run (2026-10-18, with CAP_BPF):**

- Connect, both roads: `curl http://example.com` reads 1 established on each,
  `curl localhost:1` 2 refused (v6 then v4) with errno 111 on each. In-window
  cpu: 0.5 ms on `raw_tp` against 69 ms on `fentry`, the detach the road was
  for. **Defect found and fixed**: `raw_tp` read `sport=0` on every refusal.
  `tcp_set_state(TCP_CLOSE)` runs `inet_put_port`, which zeroes `skc_num`,
  before `inet_sk_state_store` fires the tracepoint; the road now reads
  `inet_sport`, which survives it. Against a client printing
  `getsockname()`: 48928 and 34084 on both sides, established and refused.
- Accept on a launched tree: the `http.server` that `sh` started is kept
  (1 accept, `sport=8765`, `max_backlog=5`, `socketserver`'s queue size).
- pcap (2026-10-18, once `cap_net_raw` and `tcpdump` were granted):
  **defect found and fixed**. A capture of `curl localhost` (6 ms) read 0
  events while the socket said it saw 28 frames and dropped none: TPACKETv3
  hands a partly filled block over only after its 5 ms timeout, and the last
  drain ran before it. `stop()` now waits two timeouts, drains, and keeps
  the frames stamped inside the window: 28 of 28, 30 of 30, and `tcpdump
  -nr results.pcap` reads the 28. On `lo` each packet is there twice, its
  outgoing and its incoming copy, where `tcpdump -i lo` shows one (libpcap
  drops the outgoing copy on loopback); `--filter 'direction != "outgoing"'`
  is the same capture, as the reference pair reads it (1851 against 1851).

## Not done

Nothing of the stream's list. `net.tcp.lifetime` in the lazy `med` band was
done by lazy.

## Tests

`--lib collect::net net_connect accept::tests`, `--test family_net --test
catalog_model --test cli --test lazy --test encoders`, clippy, fmt. Two
`scenario_net` tests fail, the same way on `main` on this machine
(`a_net_source_this_build_does_not_collect_exits_three_and_names_the_backend`,
`the_three_answers_an_ebpf_net_request_can_be_refused_with`): no CAP_BPF.

## Decision lines for `docs/DECISIONS.md`

- A raw-tracepoint road beside a fentry one is a fallback, degraded by what it
  cannot see (an outcome, a generated cookie), and asked for by name when
  the trampolines' cost is the question.
