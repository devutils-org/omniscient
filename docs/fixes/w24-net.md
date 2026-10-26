# The twenty-fourth wave, stream net

Branch `feat/w24-net`. `docs/dev/BRIEF.md`, "### net": what the comparison against
`ip`, `nstat`, `ss`, `tcpdump` and the bpftrace tools asked of the family
(`docs/fixes/w22-net.md`, `w23-net.md`, `w22-bpftrace.md` 3 and 6). Defects
first, then features. No shared layer touched: the one new eBPF program file
(`src/bpf/net_accept.rs`) is declared from `src/bpf/net_connect.rs` with
`#[path]`, so `src/bpf/mod.rs` and `attach.rs` do not move.

## What landed

| Commit | What |
|---|---|
| `ea2ce13` | **fix**: `net.bytes` and `net.packets` take their baseline in `start()`, before the program is released; the runner's first tick passes over (as `io.bytes` does since the twenty-first wave). `dir` on `net.packets` (`fs:proc`), one series per direction. |
| `4ccb642` | **fix**: `net.iface` and `net.backlog` read again in `stop()` on a poll, which put a near-zero point at the end of every polled series -- the defect `dev.rs` had (`docs/DECISIONS.md`, "the runner closes the window"). A poll now reads nothing at `stop`; a count still closes there. Both take their baseline at `start`. |
| `36b4d20` | `direction`, `saddr`, `sport` as `net.capture` filter fields, and a caveat on `lo`'s two copies. |
| `bad8fd4` | `mcast` and `missed` on `net.iface`: `/proc/net/dev`'s receive multicast column, and sysfs's `rx_missed_errors` (absent, noted, where sysfs has none). |
| `128f945` | The rest of `tcp_info` on `net.tcp.rtt` (`netlink`), as `ss -ti` prints it, and the congestion algorithm through `INET_DIAG_CONG`. |
| `520d1c6` | **`net.snmp`**: every counter of `/proc/net/snmp`, `/proc/net/netstat`, `/proc/net/snmp6` by `nstat`'s name, `count`/`poll`/`snapshot`, `raw` only (below). |
| `165f61c` | **`net.sockets`**: the socket inventory (`ss -tuanp`) through `sock_diag`, `snapshot`/`poll`, keyed by cookie. |
| `08f7032` | **`net.tcp.lifetime` per launch and per pid** (bpftrace 3): the program records the task current at `SYN_SENT`, `FIN_WAIT1` or `LAST_ACK` (the transitions a process makes itself) and the close carries that owner (`attributed: owner`) instead of whoever the close interrupted (`at_close`). A `--pid`/launch target keeps its process tree's connections, descendants followed through `/proc/<pid>/task/*/children` at each drain; a child gone before it was seen is a counted loss. |
| `f0e8a21` | **`net.tcp.accept`** (bpftrace 5): fexit on `inet_csk_accept`, both prototypes; accepting task, both ends, the accepted socket's cookie, the listener's queue after the accept and its bound. |
| `eb484af`, `5f5b88c` | **The kernel stack on `net.drops`** (bpftrace 6): `-e stack` walks it through a stack map and names it through `/proc/kallsyms` (the tracer's two leaf frames taken off), and adds `function`, the symbol `location` is in. The stale note "names are not read from BTF" replaced with the true one. |
| `da9787b` | Tests: four ground-truth tests in `tests/family_net.rs`; three `scenario_net` tests the entries moved (`net.iface`'s seven kinds, `count x stack` refused, `net.tcp.lifetime --pid` served). |
| `7a240d4` | `docs/scenarios/net.md`: the wave's table, the bench explanation. |

## Shown vs reasoned

**Shown** on this machine, 2026-10-05, other streams running, with `make`'s
capabilities (and `make CAPTURE=1` for the capture):

| What | `omn` | Independent | |
|---|---|---|---|
| `net.packets -k iface,dir` over a pingpong, poll@100ms 2.5 s | rx 1860, tx 1860 (74.4 x 25) | `ip -s -j link` delta 1860 / 1860 | exact |
| baseline at `start` | 4 MiB pushed before the first tick are in the second tick's interval | the volume the test pushed | unit test |
| `net.capture --filter 'dport == P && direction != "outgoing"'` | 932, 916 | `sudo -n tcpdump -i lo 'tcp dst port P'`: 932, 916 captured | exact, twice |
| `net.iface` `mcast`, `missed` snapshot | 0 on all seven interfaces | sysfs `multicast`, `rx_missed_errors`; `ip -s -j link` | at zero only |
| `net.tcp.rtt` extras, one connection of 500000 bytes | rto 201 ms, rtt 38 us, minrtt 7 us, bytes_acked 500001, bytes_sent 500000, segs 15/10, pacing 33689003215 B/s, delivery 9354714285 B/s, cubic, ... | `ss -tin`: the same, rates x 8 (269512025720, 74837714280 bps) | exact |
| `net.snmp -m count` over 200 refused connects | TcpAttemptFails 200, TcpOutRsts 200, TcpActiveOpens 201, TcpInSegs 524 | `nstat`: 200, 200, 201, 527 | exact but InSegs (window) |
| `net.sockets -m snapshot --system` | 29: tcp ESTAB 20, LISTEN 5, udp ESTAB 1, UNCONN 3 | `ss -tuanH`: the same 29 by protocol and state | exact |
| `net.tcp.lifetime -- sh -c 'sleep 0.5; python3 conns.py 10'`, another process making 20 connections in the same seconds | 20 events, all `owner`, none of the other's | the 20 ends the program made; `--system` over the other process alone sees its 40 ends, all `owner` | exact |
| `net.tcp.accept --system` over six queued connections | pid, comm, ends equal; `backlog` 5..0 of 16 | `sudo -n bpftrace tcpaccept.bt`: `BL` 0/16..5/16 (the accepted socket's copy, taken at SYN) | equal, the queue read differently (caveat) |
| `net.drops -e stack` over 50 UDP sends to a closed port | 50 drops on one stack of 21 frames, `function` `udp_rcv` | `sudo -n bpftrace -e '... @[kstack, comm] = count()'`: the same 21 frames in order, 50 | exact |
| `net.tcp.connect`'s band: `-- sleep 0.2` | connect (6 trampolines) 69.2 ms cpu in-window, accept (1) 11.8 ms, lifetime (raw_tp) 2.6 ms | -- | ~11 ms a trampoline |

**Reasoned**: that the `net.tcp.connect` in-window cost is the detach at
`stop` (the window opening after `start` is the runner's documented order; I
did not time attach and detach apart). That `missed` and `mcast` agree when
non-zero (every interface here reads zero). That the lifetime owner is right
on a timer- or reset-driven close: there `attributed` is `at_close` by design,
and since the review round such a record is a counted loss under a process
target rather than placed by the task it interrupted (unit-tested on
synthetic records; no reset-driven close was produced under `--pid` here).

**Tests run**: `cargo test --lib collect::net` 117, `--lib net_` 59 (the bpf
programs), `--lib drops`/`accept` 18/19; `--test family_net` 13 (the two eBPF
ones run, not skipped); `--test scenario_net` 41 -- after the merge of main,
40 and `every_datagram_sent_to_a_closed_port_is_a_drop_with_one_reason` (in
the flaky tally), which passed alone and in the full file's next run; `cargo
clippy --workspace --all-targets -- -D warnings` and `cargo fmt --check`
clean. `--test catalog_model` 19; `--test cli` 32 and
`the_catalog_document_s_cost_column_is_the_catalog_s_chain` red on
`mem.numa` and `mem.swap` (stream mem's, from main; filed below).

The flaky drop scenario's mechanism, for whoever owns the tally: it sends its
512 datagrams 200 ms after spawning `omn`, and when startup is slower than
that under load they land before the program attaches: the failing run's
drops had no `NO_SOCKET` at all while `Udp: NoPorts` moved.

## Not done, and why (ranked)

1. **pcap output** (`tcpdump -w`): an encoder is `src/format.rs` (stream
   emit), and the frame's captured header bytes would have to travel on each
   `net.capture` event, which is this family's half and is not built. Filed
   below.
2. **`net.snmp` as series**: a counter-name key needs a `KeyKind` and a
   `KeyValue` variant (`src/request.rs` is a seam, `src/event.rs` is not this
   stream's). Until then `net.snmp` declares `raw` only, and says why.
3. **`net.tcp.connect`'s band moved**: explained, not moved. Moving it is a
   `bpf:raw_tp` backend on `inet_sock_set_state` (`SYN_SENT` to
   `ESTABLISHED`/`TCP_CLOSE`, ~2.6 ms like `net.tcp.lifetime`), degraded by
   not seeing a connect that failed before a SYN left.
4. `ss -s` (`/proc/net/sockstat`'s totals): countable from `net.sockets`'
   raw events; no source of its own.
5. `net.tcp.lifetime --tid` (the catalog derives `tid` from `pid`; the
   collector refuses it, exit 3) and its `bpf:tp` twin's collector.
6. `net.tcp.accept` follows one process, not a launched tree (as
   `net.tcp.connect` does); the caveat says so.

## Privileged runs wanted

- **A root `bpftrace` was left running** by this stream: `sudo -n bpftrace
  /usr/share/bpftrace/tools/tcpaccept.bt` (pid 204407 at the hand-off), whose
  `pkill` `sudo -n` does not grant. By a person: `sudo kill -INT 204407`. Later
  witness runs here used `bpftrace -c CMD`, which ends with the command.
- To rerun the comparisons above: `make` (and `make CAPTURE=1` for the
  capture), then the commands in the "Shown" table; `sudo -n bpftrace` and
  `sudo -n tcpdump` are granted.

## Decision lines for `docs/DECISIONS.md`

- A connection's process is its **owner**, the task current at the first
  transition a process makes on its own socket (`SYN_SENT`, `FIN_WAIT1`,
  `LAST_ACK`), as `tcplife` attributes it; the task at `TCP_CLOSE` only where
  none was seen, and the event says which (`attributed`).
- A polled counter source reads nothing in `stop()`; only a `count` closes
  there (the runner's last tick is the closing read). `net.iface` and
  `net.backlog` now follow it, with `dev.rs`, `io` and `power`.
- A counter that did not move is absent from a `net.snmp` interval, as in
  `nstat`; a snapshot lists every counter.

## Needs from another stream

- **emit** (`src/request.rs` seam, `src/event.rs`): a key naming a counter
  (`counter:TcpRetransSegs`), so `net.snmp` can declare `rate`, `summary`,
  `timeline`; and a `pcap` encoder in `src/format.rs`, for which net will add
  the captured header bytes to each `net.capture` event.
- **reference** (`catalog/references/net.yaml`, `docs/replaces/net.md`):
  `tcpdump-captured` can read `omn` through the filter `direction !=
  "outgoing"` instead of the `omn_captured` parser; new pairs are possible for
  `nstat` (any counter through `net.snmp`), `ss -tuan` (`net.sockets`), `ss
  -ti`'s other fields, `tcpaccept`, `tcplife` per launch, and the drop
  stacks; the replaces rows for `nstat`, `ss`, `ss -ti`, `ip -s link`
  packets (`dir`) and errors (`missed`, `mcast`) move from "in part"/"not" to
  replaced.
- **lazy** (`catalog/net.yaml`'s `default:` block): `net.tcp.lifetime` now
  serves a launch, so `med` could compose it beside `net.socket` and
  `net.tcp.connect`.
- **mem** (or the coordinator): `cli::the_catalog_document_s_cost_column_is_the_catalog_s_chain`
  is red on main's `docs/CATALOG.md:311` (`mem.numa`) and `:315` (`mem.swap`).
- **coordinator**: `docs/CATALOG.md` and `man/omn.1` rows for `net.snmp`,
  `net.sockets`, `net.tcp.accept`; `net.tcp.lifetime`'s targets (`pid`,
  `launch`); `dir` on `net.packets`; `net.iface`'s `mcast`/`missed`;
  `net.drops`' `stack` emit; `net.capture`'s new filter fields.

## Review

Both findings were right; both are fixed in the review round.

1. **`net.sockets --pid` on an unreadable process** (defect). `attach`
   now refuses a `--pid` whose `/proc/<pid>/fd` answers `EACCES`, with the
   reason and the way out (`--system`, where its sockets carry `uid` and no
   `pid`); a process that becomes unreadable after attach (a setuid exec) is
   a `SampleError::Transient`, so a failed read in `Losses`, never an empty
   inventory. Shown: `omn net.sockets --pid 1 --mode snapshot -e raw` as an
   unprivileged user now exits 1 with "cannot read /proc/1/fd (Permission
   denied) ... which sockets it holds is unknown". The catalog caveat says so.
   Tests: `sockets::tests::a_process_whose_descriptors_cannot_be_read_is_not_an_empty_inventory`
   (attach refusal and the sample path, both run here) and
   `scenario_net::the_sockets_of_a_process_whose_descriptors_are_unreadable_are_refused_not_empty`
   (the reviewer's repro through the binary).
2. **`net.tcp.lifetime` `at_close` records under a process target** (gap).
   `LifetimeCollector::in_target` places a connection only by an `owner`
   pid; an `at_close` one under `--pid`/launch is counted in `unowned`, in
   `Losses::dropped` with a note, and in the run's notes -- never kept because
   the interrupted task was the target, nor counted `outside` because it was
   not. Machine-wide nothing changes. Test:
   `lifetime::tests::an_unowned_connection_under_a_process_target_is_a_loss_not_a_placement`
   (synthetic records: one at_close with the target's pid, one with a
   stranger's, both lost, `outside` 0; an owned one placed by its pid). The
   catalog caveat says so, and its drain interval is corrected to 20 ms (it
   said 10).

Tests of the round: `cargo test --lib collect::net` 119; `--test
scenario_net` 42; `--test family_net` 13; `--test catalog_model` 19;
`cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --check`
clean.
