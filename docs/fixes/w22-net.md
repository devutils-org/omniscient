# Wave 22, stream net -- handoff

Branch `feat/w22-net`. `ip -s link`, `nstat`, `ss -ti` and `tcpdump`'s
counts against the net sources.

## What landed

- `catalog/references/net.yaml`: nine pairs. Two fields are the family's
  own and documented in the file's header: `alongside: {lead_ms}` and the
  `compare.omn` paths (`series[<key>].sum|mean`, `events.count`,
  `events.<attr>.sum`).
- `tests/reference/workloads/net/`: `push.py` (a known volume over
  loopback), `pingpong.py` (one connection of small round trips on a fixed
  port), `backlog.py` (a one-deep accept queue given 64 connections); and
  three wrappers that make a tool measure one run: `around.py` (read a
  counter tool before and after), `sample.py` (a snapshot tool while the
  workload lives), `bracket.py` (a tool that tallies until SIGINT).
- `tests/reference/net.rs`: four parsers (`ip_s_link_delta`,
  `nstat_delta`, `ss_ti_rtt`, `tcpdump_count`), unit-tested on
  `tests/reference/captures/net/` (the first three captured on this machine
  through the wrappers; `tcpdump_count.stderr.txt` written by hand in
  tcpdump 4.99's tally format, since tcpdump is not installed); and
  `net_pairs_against_the_tools`, which runs every net pair the family's way
  until the shared runner knows `alongside`.
- `docs/replaces/net.md`: fourteen rows.

## Shown vs reasoned

**Shown** (`cargo test --test reference net:: -- --nocapture`, 2026-10-04,
with the capabilities granted by `make`):

| Pair | omn | tool | tolerance |
|---|---|---|---|
| `net.bytes / ip -s link [rx_bytes]` | 67196604 | 67196604 | rel 3 % |
| `net.bytes / ip -s link [tx_bytes]` | 67197020 | 67197020 | rel 3 % |
| `net.packets / ip -s link [packets]` | 7452 | 7452 | rel 5 % |
| `net.iface / ip -s link [rx_errors]` | 0 | 0 | abs 0 |
| `net.backlog / nstat [TcpExtListenOverflows]` | 62 | 62 | abs 2 |
| `net.backlog / nstat [TcpExtListenDrops]` | 62 | 62 | abs 2 |
| `net.tcp.retransmit / nstat [TcpRetransSegs]` | 124 | 124 | abs 8 |
| `net.tcp.rtt / ss -ti [rtt_ns_mean]` | 1068750 | 1068750 | rel 15 % |
| `net.capture / tcpdump [captured]` | skip: needs root, `sudo -n` does not grant it | | |

The first run of the packets pair used the 64 MiB push and failed, 3692
against 3502: ~1700 64 KiB segments per direction are few enough that the
other streams' loopback traffic in omn's wider window was 5.4 %. It now
uses `pingpong.py` (thousands of small segments) and is exact. The bytes
pairs passed by 0.08 % on that same loaded run.

**Reasoned**: that the tolerances hold on a loaded machine. Each pair's
`why` says what can land on omn's side only (omn's window is wider than
the tool's two readings), and the bound is sized for it; one run here came
out exact, which says nothing about the worst case. `net.iface` agreeing at
zero does not show a non-zero error count would agree.

## Not done, and why

- The tcpdump pair has not run: tcpdump is not installed, it needs root,
  and omn's `net.capture` needs `CAP_NET_RAW`.
- No pair for `ip -s -s link`'s detailed errors through `netlink:ethtool`
  (`ethtool` is not installed, and lo has none to show).
- Rows without a pair, because omn has no source to compare (the gaps
  below): `nstat`'s other counters, `ss -tan`/`-tlnp`, `ss -s`,
  `tcpdump -w`.

## Gaps found (each a line of work for omn)

- `net.packets` has no `dir` key: it sums received and sent, where
  `net.bytes --keys iface,dir` splits them. `ip -s link` reports them apart.
- A generic SNMP counter source (`net.snmp`): every `/proc/net/snmp` and
  `/proc/net/netstat` counter by name, in `count`/`poll`/`snapshot`, which
  is what `nstat` and `netstat -s` are mostly used for.
- A socket inventory source: every socket with state, `Recv-Q`/`Send-Q`
  and owning process, in `snapshot` (what `ss -tan`, `ss -tlnp` are for),
  and its summary by protocol and state (`ss -s`, or `/proc/net/sockstat`).
- `net.tcp.rtt` carries rtt, rttvar, cwnd, ssthresh, retransmits and the
  congestion state; `ss -ti` also prints `bytes_acked`, `bytes_received`,
  `delivery_rate`, `pacing_rate`, `busy`, `minrtt`, `rto`, `unacked` and
  the congestion algorithm, all in the same `tcp_info` the netlink backend
  already reads.
- `net.iface` drops `ip -s link`'s `missed` and `mcast` columns.
- A `pcap`/`pcapng` output format for `net.capture` (`tcpdump -w`).
- omn refuses a program beside `--system` or `--iface`
  (`the argument '--system' cannot be used with '[PROGRAM]...'`). For a
  counter source, "run this program, and report the machine's increase over
  its life" is the common use (`perf stat -a -- prog` does it); today it
  takes a `--duration` guessed to cover the program.

## Privileged runs wanted

```console
sudo pacman -S tcpdump ethtool
make CAPTURE=1      # CAP_NET_RAW on target/debug/omn for net.capture
# then, with a sudo that does not prompt (sudo -v first, in a terminal):
cargo test --test reference net::tests::net_pairs_against_the_tools -- --nocapture
```

Note `cargo test` relinks `omn` only when `src/` changed; if it does, run
`make` (and `make CAPTURE=1`) between `cargo test --no-run` and the run, as
the `net.tcp.retransmit` pair needs `CAP_BPF`/`CAP_PERFMON` too.

## Decision lines

- A net pair runs **alongside**: omn for a `--duration` with no `--`, the
  tool's wrapper starting the workload `lead_ms` later. The same run of the
  workload for both; the asymmetry (omn's window wider) is stated in every
  `why`.
- Tools that print totals are made to measure one run by reading them
  before and after the workload (`around.py`), never by subtracting two
  separate runs.

## Requests to other streams

- **reference**: honour `alongside: {lead_ms}` in the shared runner (start
  omn without `--`, start the reference command `lead_ms` later, wait for
  both), and the `compare.omn` path forms net.yaml's header lists -- or
  leave net's pairs to `net_pairs_against_the_tools` and say so. As it
  stands, a runner that appends `-- <workload>` to omn's args gets a clap
  refusal for every net pair. Once the runner does it, the runner part of
  `tests/reference/net.rs` (`mod runner`) can go.
- **reference**: `docs/replaces.md`'s net line: fully replaced --
  `ip -s link` bytes, `nstat` listen overflows/drops and `TcpRetransSegs`,
  `ss -ti`'s rtt; in part -- `ip -s link` packets and errors, `ss -ti`'s
  other fields, `tcpdump`'s counts (unrun); not -- the rest of `nstat`,
  `ss`'s socket listing and totals, `tcpdump -w`.
- **reference**, `.github/expected-skips*.txt`: the skip is printed by
  `net_pairs_against_the_tools` as `net pair SKIP  net.capture / tcpdump
  [captured]: ...`, not as an ignored test, so it may need no entry.
