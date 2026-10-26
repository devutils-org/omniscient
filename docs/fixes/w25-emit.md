# Stream emit, the twenty-fifth wave

Branch `feat/w25-emit`. Done sequentially by one agent.

## Outside the owned files, said first

- `src/encode.rs` (the encoders): the pcap encoder, `unmeasured` under rate
  and timeline in text, `sums` on the summary line. Not in the row's list,
  but it is the emit layer's encoder file and nobody else's.
- `src/cli.rs` (run's): `--value ATTR` and `--sum ATTR[,ATTR]` on `omn trace`.
- `src/resolve.rs` (lazy's): `check_filter_fields` also checks a named value
  and the summed attributes against the backend, as it checks `@<attr>`.
- Every `Request { .. }` literal gained `value: None, sum: Vec::new()`
  (34 sites, tests included).

## What landed

1. **Defect: `filter.rs` read the event's kind before an attribute named
   `kind`.** `--filter 'kind == "soft"'` on `sched.irq` compared `sched.irq`
   and kept nothing. An attribute named `kind` now comes first.
2. **Defect: timeline and rate refused a run holding an event without its
   value** (`power.freq` through `pmu` on a cpu that never left idle). They
   count it as `unmeasured` now, as summary, hist, topk and stack do; text
   says "and N events that carried no value".
3. **Keys `op`, `level`, `mapping`, `user`, `counter`**: derived keys, each
   read off the event attribute it is named after (`op:vfs_read`). A family
   puts the attribute on its events and adds the key to its entry's
   `keys:`; nothing else. Not yet declared by any entry (io, mem, net own
   those lines).
4. **`--value ATTR`**: the aggregating emits reduce that attribute instead of
   the collector's own, in the unit its name says (`_ns`, `bytes`, `hz`,
   `busy`/`ratio`/`share`, else count), with a degradation line naming it.
   Validated against the entry's `filter_fields` and the backend's.
5. **`--sum ATTR`**: each summary row also adds up the named attributes
   (`sums: {errors: 3}` in JSON, `errors 3` in text):
   `proc.syscall -k syscall -e summary --sum errors` is `strace -c` with its
   errors column. Refused without a summary.
6. **`within` key, the inclusive share** (`perf report --children`): each
   sample counts once in the row of every distinct function on its stack;
   raw events and `events_seen` stay the run's; a degradation line says the
   rows add up to more than the run. Refused with `--stack none`.
7. **`--format pcap`**: nanosecond pcap of raw events carrying `header`
   (hex of the captured bytes), `incl_len` the bytes, `orig_len` the frame's
   `bytes`, time from `realtime_ns` when present (read as an integer) or the
   monotonic stamp moved onto the realtime clock; one link type per file
   (`linktype`, Ethernet by default). A run without frames is refused with
   that reason.

## Shown vs reasoned

- `--sum` and `--value` (shown, on `mem.heap.alloc` through the preload
  shim): `--value freed_bytes` summed 4.1 MiB, `--sum freed_bytes` beside
  the `bytes` value 4 262 858 B (4.07 MiB) on another run of the same
  program. `--value nope` is refused with "did you mean `op`".
  `proc.syscall` against `strace -c` is not shown: `proc.syscall` needs
  CAP_BPF, which this machine does not grant.
- pcap (reasoned, and the bytes checked by a unit test against the format's
  layout): no `tcpdump` or `tshark` on this machine, and `net.capture` needs
  CAP_NET_RAW. Wanted: `omn trace net.capture --iface lo -e raw --format
  pcap -o /tmp/cap -- curl -s localhost` then `tcpdump -r` on the file,
  once net fills `header`.
- The others by unit test.

## Not done

1. **An `object` key** (perfrecord 4): needs the unwinder to keep every
   frame's object (unwind's `src/stack.rs`). Not started.
2. `omn list --format json` carrying the defaults and each backend's
   `emits`: done in stream lazy.

## Needs from another stream

- **net**: `header` (hex) on `net.capture`'s events, and `realtime_ns` (the
  kernel's stamp, an integer) and `linktype` when the socket is not
  Ethernet; `counter` in `net.snmp`'s keys (its rows carry `name` today:
  either a `counter` attribute or rename).
- **io**: `op` in `io.vfs`'s `keys:`.
- **mem**: `level` in `mem.latency`'s `keys:`; `mapping`/`user` attributes
  and keys on the memory sources that have them.
- **cpu**: `func` and `within` in `cpu.profile`'s `keys:`.

## Tests

`cargo test --lib` for emit, encode, format, filter, validate, resolve, cli;
`--test aggregation --test join_engine --test encoders --test cli --test
recorder --test catalog_model`; clippy; fmt.

## Decision lines for `docs/DECISIONS.md`

- Keys read off an attribute are derived keys: a family adds the attribute
  and the key's name to `keys:`, and needs no `KeyValue` variant.
- `within` is the one key that puts an event in several series; raw output
  and `events_seen` are the run's own events.
- `--value` changes the unit by the attribute's name, and says so.
