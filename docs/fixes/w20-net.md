# The twentieth wave, stream net

Item 8's network half: the network sources the views found short. Three of
the five asks landed whole, one landed as far as this stream's files reach,
and one is blocked on a file another stream owns.

## What landed

- **`net.socket` carries its route's interface** (`1b38274`). The exit
  program reads `sk->sk_dst_cache->dev->ifindex` into the histogram row's
  `dev` (`src/bpf/net_socket.rs`, `route_interface`), after comparing the
  socket's network namespace (`skc_net` through `possible_net_t`, then
  `ns.inum`) with this process's. The collector turns a non-zero index into
  an `iface` key, as the third key after `sock` and `task`, plus an `iface`
  attribute with the name. Names are looked up once per index per run. The
  catalog lists `iface` in the entry's `keys` and `filter_fields`, and adds a
  caveat. A unix socket, an unconnected datagram socket and a socket in
  another namespace carry no interface. They get no key rather than a zero.
- **`net.bytes` accepts `raw`** (`1b38274`). Each event already carried `rx`
  and `tx` beside `bytes`. With `raw` allowed, the split survives to the
  results instead of living only in a live run. That is the "in apart from
  out: in the events only" that `docs/view/system.md` reports. A caveat
  says a series is both directions together.
- **`net_qdisc`'s namespace offset goes through `member_offset`**
  (`adcf0e0`). The program now reads at `nd_net + possible_net_t.net`
  instead of at `nd_net` with an assumed zero. A kernel whose
  `possible_net_t` has no `net` member (no `CONFIG_NET_NS`) reads no
  namespace instead of failing.
- **`net.request`'s never-ended, unborn and replaced counts as numbers**
  (`f7075f5`): `RequestCollector::undrawn() -> Undrawn`. The struct holds
  `never_ended`, the first five as `(key, tid)`, `unborn`, `replaced` and
  `filtered_out`, and derives `serde::Serialize`. The notes are now written
  from this struct, so the sentence and the number cannot disagree.
  **This is only half the ask.** The result has no slot for a collector's
  fields, and the slot is in files this stream does not own. See "Needs
  from another stream".

## Shown versus reasoned

- **Shown, on the kernel, with `cap_bpf,cap_perfmon` on `target/debug/omn`**:
  `scenario_net::every_send_and_receive_is_found_on_the_socket_that_made_it`
  now asserts that every row of both the client's and the server's loopback
  TCP sockets carries `iface == "lo"`. It passed (not skipped), so the
  verifier accepts the route walk and the walk reads the correct index.
- **Shown**: `family_net::a_raw_run_keeps_the_bytes_in_apart_from_the_bytes_out`.
  8 MiB was pushed over `lo` under `--emit raw`. The run had `rx >= 8 MiB`,
  `tx >= 8 MiB`, and `bytes == rx + tx` summed over the events.
- **Shown**: `net_qdisc::the_namespace_pointer_is_found_through_possible_net_t`
  (the offset the program uses is `nd_net` plus the typedef's member), and
  the other nine `net_qdisc` unit tests, which build the programs against
  this kernel.
- **Shown**: `collect::net::request::what_is_in_no_event_is_counted_as_numbers_and_said_from_them`
  (6 never-ended with 5 named, 2 unborn, 1 replaced, and the three sentences
  that start with those numbers).
- **Reasoned**: a socket in another namespace gets no interface. The
  comparison is in the program and the test checks that it comes before
  the write, but no run used a second namespace. The route can change during
  a socket's life (a re-route, `IP_UNICAST_IF`). A row then carries the
  route held at the call, which is a new row key. The cardinality cost is
  reasoned, not measured.
- **Reasoned**: on this machine `possible_net_t.net` is at offset 0, so the
  qdisc fix changes no instruction here. It matters only on a kernel that
  lays the struct out differently.

## What I could not do, and why

- **`net.request`'s bytes per request and the start of its longest sleep.**
  Both need new fields in the task storage and in the record. Those are
  `STORAGE_BYTES`, the `S_*` and `R_*` offsets and `RequestRecord` in
  `src/bpf/request.rs`, which belongs to stream io, so the request is filed
  below. Once those fields exist, the work in this stream's files is
  `birth_socket` and `end_socket` in `request_net.rs` and two attributes
  in `event()` in `collect/net/request.rs`.
- **The counts as fields of the result.** `Results` (`src/emit.rs`, view's)
  has no per-collector field map, and `Collector` (`src/collect/mod.rs`,
  budget's) has no hook to fill one. Filed below. The struct is ready to be
  handed over.
- **`net.bytes` "split by direction" as separate series.** I did not split
  the series. Keying `lo/rx` and `lo/tx` (the sub-series form `net.iface`
  uses) would change every `net.bytes` series a view, a join and a test
  reads today. The split is in the events and now reaches the results under
  `raw`. A decision line is below.

## Privileged runs wanted

None beyond what I ran. `make` granted the capabilities in this worktree
without a prompt. One run would be worth doing, to show what is now only
reasoned (namespace creation needs `CAP_SYS_ADMIN`):

```console
sudo -n ip netns add omn-w20 && sudo -n ip netns exec omn-w20 ip link set lo up
# then: a TCP pair inside omn-w20 under
target/debug/omn net.socket --system --mode trace --emit raw --duration 2s --format json
# expect: that pair's rows carry no `iface`; this namespace's still `lo`.
sudo -n ip netns del omn-w20
```

## Decision lines for DECISIONS.md

- **`net.socket`'s interface is the route's, read at the call, in this
  namespace only.** Another namespace's index would name some other
  interface here, so such a row carries none. The alternative was to carry
  the namespace beside the index, which needs a wider row key in
  `src/bpf/fentry.rs` (shared). Leaving that open costs nothing until a
  container's sockets are measured from the host.
- **`net.bytes` direction stays an attribute, not a series** (proposal).
  `raw` carries it to the results. Splitting the series would break every
  reader of `iface:lo`'s series. If a view needs series per direction, it
  should ask for them as an emit over an attribute, not as a change to the
  collector's key.
- **A collector's run-level counts reach the result as `fields`** (proposal,
  filed below): one `BTreeMap<String, u64>` on `Results`, filled by a
  `Collector::fields()` default-empty hook. Until it exists, the view keeps
  parsing `net.request`'s notes (`view/src/views/request.rs`, `Undrawn::of`).

## Needs from another stream

- **io (`src/bpf/request.rs`)**: four fields for `net.request`.
  1. Storage `S_RX_BYTES` and `S_TX_BYTES` (u64 each), with the record
     fields `rx_bytes` and `tx_bytes` that the end copies. `request_net.rs`
     fills them from `tcp_sock.copied_seq` at the birth and the end, and
     from `write_seq` at the birth and the end (it already reads `write_seq`
     at the end).
  2. Storage `S_HOLDER_SINCE` and record `holder_since_ns` (u64): the
     moment the longest interruptible sleep began. It is written in
     `settle` where `S_HOLDER_NS` is replaced, using the off-cpu stamp
     `S_OFF_SINCE`.
  `RECORD_BYTES` grows from 168 to 192 and `STORAGE_BYTES` from 144 to 168.
  The collector test `with_socket` in `collect/net/request.rs` builds a
  168-byte record and will need its length bumped (this stream's file, a
  one-line follow-up).
- **budget (`src/collect/mod.rs`) and view (`src/emit.rs`)**: a
  `Collector::fields(&self) -> BTreeMap<String, u64>` hook (default empty),
  copied into a `Results::fields` (`#[serde(default, skip_serializing_if =
  "BTreeMap::is_empty")]`). `net.request` then returns `never_ended`,
  `unborn`, `replaced` and `filtered_out` from `undrawn()`, and the view's
  `Undrawn::of` reads them in place of the sentences.
- **view (`docs/view/README.md` needs table, `view/src/views/system.rs`)**:
  `net.bytes` takes `raw` now. The system view's network row can ask for it,
  and the "in apart from out" split then survives the end of the run.
  `docs/view/system.md`'s "The catalog refuses `raw` for four of these
  sources" drops `net.bytes`.
- **docs (`CATALOG.md`)**: in the `net.socket` row, the interface as a key.
  In the `net.bytes` row, `raw`.
- **coordinator (flaky tally)**:
  `scenario_net::every_datagram_sent_to_a_closed_port_is_a_drop_with_one_reason`
  failed twice here, once in the file's run and once alone (*"only 13 drops
  were reported"*), with nine agents on the machine. It is already in the
  tally twice. No file it reads changed in this stream.

## The suite

- `cargo test --lib net_qdisc`: 10 passed. `--lib net_socket`: 2 passed.
  `--lib collect::net::socket`: 3 passed. `--lib collect::net::request`: 9
  passed. `--lib catalog`: 59 passed.
- `cargo test --test family_net`: 9 passed.
- `cargo test --test scenario_net`: 39 passed and 1 failed (the flaky test
  above, failed again when rerun alone).
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
  `cargo fmt --check`: clean.

## Review

The reviewer's one finding is a gap and not a defect. It names the two asks
this stream did not deliver, `Undrawn` as fields of the result and
`net.bytes` as one series per direction, and the namespace case that is
only reasoned. After merging main (the io stream included) I checked
again. Neither prerequisite has landed. `src/bpf/request.rs` still has
`STORAGE_BYTES = 144` and `RECORD_BYTES = 168`, with no
`S_RX_BYTES`/`S_HOLDER_SINCE`. `Collector` has no `fields()` hook, and
`Results` has no run-level counts map. Both are in files other streams own,
so I made no change in code this round. What is left goes to the coordinator:

1. **Route to io**: the four `src/bpf/request.rs` fields, as specified under
   "Needs from another stream". The follow-up in this stream's files is
   `birth_socket`/`end_socket` in `request_net.rs`, two attributes in
   `collect/net/request.rs`'s `event()`, and the length in `with_socket`.
2. **Route to budget and view**: `Collector::fields()` plus `Results::fields`.
   When they land, `net.request` returns `undrawn()`'s four counts from the
   hook. That is a few lines, because the struct already exists and the
   notes are written from it.
3. **Settle the decision line** "`net.bytes` direction stays an attribute,
   not a series". If DECISIONS.md takes the other side, the re-key is a
   change in this family, but every `net.bytes` reader must move with it in
   one commit.
4. **The other-namespace case** stays reasoned until someone does the
   privileged run under "Privileged runs wanted". That run needs
   `CAP_SYS_ADMIN` to create a namespace, which this worktree's capability
   step does not grant.
