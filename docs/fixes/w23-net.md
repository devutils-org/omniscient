# The twenty-third wave, stream net

Branch `feat/w23-net`. The net family's eight pairs, run by the shared
runner: `run: alongside`, the tools' wrappers measuring the same window as
`omn`'s `--duration`, and `omn`'s side read by a path or by an `omn_parse`
parser. The family's own harness (`mod runner`,
`net_pairs_against_the_tools`) is gone.

## What landed

- `catalog/references/net.yaml`: eight pairs, each with an `id`, all
  `run: alongside` with `settle_ms: 1000`; no `gap:` left. The nine compares
  of the twenty-second wave are kept (the `ip -s link` error pair now
  compares four keys: errors, drops, carrier, collisions; the two `nstat`
  backlog pairs are one pair with two compares).
- `tests/reference/workloads/net/`: `around.py`, `sample.py` and
  `bracket.py` no longer run the workload (`TOOL... -- WORKLOAD`) but measure
  a window of SECONDS (`around.py 4 ip -s link show dev lo`), since under
  `alongside` the runner starts the workload. The two windows are now the
  same length, where the twenty-second wave's tool window sat inside omn's.
- `tests/reference/net.rs`: the four tool parsers, plus four parsers of
  `omn`'s output -- `omn_ip_link` (a counter series' increase, mean x count,
  in `ip -s link`'s keys: `rx_bytes`, `packets`, `errors`, ...), `omn_total`
  (every series summed: retransmits are one series per socket),
  `omn_rtt` (a mean weighted by each series' polls), `omn_captured` (raw
  events counted as tcpdump counts them, each frame on `lo` once, for
  tcpdump's `captured`; `omn_events` until the fix round, below). `ip_s_link_delta` also gives
  `errors` and `drops` (rx + tx) and `carrier`, `collisions` (tx), what
  `net.iface` counts. Each parser is unit-tested on a capture; the `omn`
  ones on seven real `omn` outputs captured today
  (`tests/reference/captures/net/omn_*.json`), `net.capture`'s among them
  since the fix round.
- The backlog pair reads `omn` by path, no parser:
  `results.0.outputs[emit=raw].events.*.attrs[name=overflows].value`.
- `docs/replaces/net.md`: rows name the pairs by id, with the runner's
  numbers.

## Shown vs reasoned

**Shown** (`make`, then `cargo test --test reference pairs_net --
--nocapture`, twice, 2026-10-04, other streams running):

| Pair | Run 1 omn / tool | Run 2 omn / tool | Tolerance |
|---|---|---|---|
| `ip-link-rx-bytes` | 67196396 / 67196396 | 67192444 / 67192444 | rel 3 % |
| `ip-link-tx-bytes` | 67196344 / 67196344 | 67195148 / 67195148 | rel 3 % |
| `ip-link-packets` | 7264 / 7264 | 7028 / 7028 | rel 5 % |
| `ip-link-errors` (errors, drops, carrier, collisions) | 0 / 0 each | 0 / 0 each | abs 0 |
| `nstat-listen-overflows` (overflows, drops) | 62 / 62, 62 / 62 | 62 / 62, 62 / 62 | abs 2 |
| `nstat-retrans-segs` | 124 / 124 | 125 / 125 | abs 8 |
| `ss-ti-rtt` | 1066000 / 1070400 ns | 1079385 / 1216800 ns (11 %) | rel 15 % |
| `tcpdump-captured` | skipped: `sudo -n` does not grant root | skipped | rel 1 % |

Tally both times: `7 held, 1 skipped, 0 gaps, 0 known defects, 0 failed`.

The rtt pair's 11 % on run 2 came from `ss` sampled every 500 ms (five
readings, one high) against `omn`'s thirteen polls; `ss` is now sampled
every 200 ms like `omn`'s poll, and that pair alone
(`OMN_REFERENCE_PAIR=ss-ti-rtt`) then read 1154000 / 1233250 (6.4 %) and
1202538 / 1162333 (3.5 %). The tolerance was not changed.

Also shown: `cargo test --test reference net::` (10 parser tests pass, 11
after the fix round);
`every_pair_is_well_formed` and `every_row_names_a_pair_or_why_none` name
no net pair and no net row (both still red on other families);
`families_are_whole` passes; `cargo clippy --workspace --all-targets -- -D
warnings` and `cargo fmt --check` clean.

**Reasoned**: that the windows' slack (tens of milliseconds between omn's
and the wrapper's first and last reads) stays inside the tolerances on a
loaded machine; two runs were exact on every counter. `net.iface` agreeing
at zero does not show a non-zero error count would agree. That the runner
would hold the tcpdump pair: it still skips there (below), and the
agreement is shown by hand only.

## Each pair's outcome

- holds: `ip-link-rx-bytes`, `ip-link-tx-bytes`, `ip-link-packets`,
  `ip-link-errors`, `nstat-listen-overflows`, `nstat-retrans-segs`,
  `ss-ti-rtt`.
- skipped by the runner: `tcpdump-captured` -- the runner asks whether
  `sudo -n` grants root, and it grants only the listed commands
  (`/usr/bin/tcpdump` among them). Run by hand in the fix round, it agrees
  exactly ("Review", below).
- gaps, known defects: none.

## Not done, and why

- The tcpdump pair has not run through the runner (its root check wants
  `sudo -n` to grant everything; that is the runner's, not this stream's).
  `tcpdump_count.stderr.txt` is now a real tally (fix round).
- The runner's `lead_ms` (the tool started after omn) was not needed:
  started together and both measuring the same seconds, the windows match
  better than with a lead.
- The twenty-second wave's gaps stand (`docs/fixes/w22-net.md`, "Gaps
  found"): `net.packets` with no `dir`, no `net.snmp`, no socket inventory,
  `ss -ti`'s other `tcp_info` fields, `missed`/`mcast`, no pcap output, and
  omn refusing a program beside `--system`/`--iface`.

## Privileged runs wanted

```console
make CAPTURE=1      # CAP_NET_RAW on target/debug/omn for net.capture
sudo -v             # by a person, in a terminal, so that sudo -n grants root
OMN_REFERENCE_PAIR=tcpdump-captured cargo test --test reference pairs_net -- --nocapture
```

The fix round ran the rest (the real tally, the `net.capture` output) with
what `sudo -n` grants today.

## Review

The reviewer: `omn_events`, the `omn_parse` of `tcpdump-captured`, was
tested only on `net.backlog`'s raw output, never on `net.capture`'s. Right,
and fixing it found that the pair as written would have failed.

- With `make CAPTURE=1` (the sudoers line now grants `cap_net_raw`), a
  real `net.capture` raw output over a 2 ms pingpong is
  `tests/reference/captures/net/omn_net_capture.json`: 14 events, equal to
  its `events_seen` and its filter's `kept`, so nothing is capped or split.
  A 2 s pingpong gave 3640 events, also equal to both (not kept: 5.7 MB).
- But the 14 are 7 frames seen twice, `direction outgoing` and
  `direction host`: lo hands a packet socket every frame twice
  (`docs/open-findings.md`, "Loopback carries every frame twice"), omn keeps
  both, and libpcap drops the outgoing copy on a loopback device. By hand,
  the pair's own commands with `sudo -n tcpdump` (granted), 2026-10-04,
  twice: omn 3562 events against tcpdump's 1781 packets captured, 3608
  against 1804 -- exactly twice, where the compare's tolerance is 1 %.
  tcpdump's own tally says the same: 3608 "received by filter", 1804
  "captured".
- So the parser is now `omn_captured`: it counts the raw events, except an
  `outgoing` one on `lo`. Run 2's omn output counts 1804 that way, equal to
  tcpdump's. Unit-tested on the new capture (14 events, 7 counted) and on
  the backlog capture (no `direction`: 5 counted, as before).
- `tcpdump_count.stderr.txt` is run 2's real stderr, replacing the
  hand-written one.
- Not shown: the runner holding the pair. Its root check skips it here
  (`skipped: ... sudo -n does not grant root`), since sudo grants
  `/usr/bin/tcpdump` and not everything. A check by command would run it;
  that is the runner's code, outside this stream.

Not an omn defect: the two frames are real and distinguishable by
`direction`, which `net.capture` reports. It is a difference a tcpdump user
replacing it would meet, and it is now in `docs/replaces/net.md`. That
`--filter` cannot name `direction` (`net.capture` exposes iface, protocol,
dport, daddr) is why the parser filters instead of omn.

## Decision lines

- Every net pair runs `alongside`, the tool through a wrapper over omn's
  `--duration` in seconds, both started together and the workload
  `settle_ms` (1 s) later.
- omn's counter totals are read by parsers that give the tool's key names
  (`rx_bytes`, `errors`, `TcpRetransSegs`), so each compare is one key on
  both sides; `ip_s_link_delta` in turn gives the rx + tx sums that
  `net.packets` and `net.iface` count.
- A path where omn holds the value at one place (the backlog's raw
  attributes); a parser for mean x count, which the grammar has no
  arithmetic for.

## Needs from another stream

- **Coordinator** (`docs/replaces.md`): net -- fully replaced: `ip -s link`
  bytes, `nstat` listen overflows/drops and `TcpRetransSegs`, `ss -ti`'s
  rtt; in part: `ip -s link` packets (no `dir`) and errors (shown at zero
  only), `ss -ti`'s other fields, `tcpdump`'s counts (unrun); not: the rest
  of `nstat`, `ss`'s socket listing and totals, `tcpdump -w`.
- **src (later)**: a counter's total in `summary` (today mean x count,
  which is what `omn_ip_link` and `omn_total` compute) would let these
  pairs read a path.
