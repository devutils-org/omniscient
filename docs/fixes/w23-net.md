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
  `omn_rtt` (a mean weighted by each series' polls), `omn_events` (raw
  events counted, for tcpdump's `captured`). `ip_s_link_delta` also gives
  `errors` and `drops` (rx + tx) and `carrier`, `collisions` (tx), what
  `net.iface` counts. Each parser is unit-tested on a capture; the `omn`
  ones on six real `omn` outputs captured today
  (`tests/reference/captures/net/omn_*.json`).
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

Also shown: `cargo test --test reference net::` (10 parser tests pass);
`every_pair_is_well_formed` and `every_row_names_a_pair_or_why_none` name
no net pair and no net row (both still red on other families);
`families_are_whole` passes; `cargo clippy --workspace --all-targets -- -D
warnings` and `cargo fmt --check` clean.

**Reasoned**: that the windows' slack (tens of milliseconds between omn's
and the wrapper's first and last reads) stays inside the tolerances on a
loaded machine; two runs were exact on every counter. `net.iface` agreeing
at zero does not show a non-zero error count would agree. `omn_events` is
tested on a real `net.backlog` raw output, not on `net.capture`'s, which
this session cannot produce (`CAP_NET_RAW`).

## Each pair's outcome

- holds: `ip-link-rx-bytes`, `ip-link-tx-bytes`, `ip-link-packets`,
  `ip-link-errors`, `nstat-listen-overflows`, `nstat-retrans-segs`,
  `ss-ti-rtt`.
- skipped: `tcpdump-captured` -- `sudo -n` does not grant root (tcpdump
  4.99.7 is installed). Beyond the sudo, omn's `net.capture` needs
  `CAP_NET_RAW` (`make CAPTURE=1`).
- gaps, known defects: none.

## Not done, and why

- The tcpdump pair has not run, and `tcpdump_count.stderr.txt` is still
  the hand-written one: both need root (below).
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
sudo -v             # by a person, in a terminal, so that sudo -n grants
OMN_REFERENCE_PAIR=tcpdump-captured cargo test --test reference pairs_net -- --nocapture
# a real tally for the parser's capture, to replace the hand-written one:
(sleep 1; python3 tests/reference/workloads/net/pingpong.py 47312 2 >/dev/null) &
sudo -n python3 tests/reference/workloads/net/bracket.py 5 tcpdump -i lo \
  -w /dev/null 'tcp dst port 47312' \
  2> tests/reference/captures/net/tcpdump_count.stderr.txt
```

Then move `tcpdump_count_reads_the_tally_on_stderr`'s numbers to it.

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
