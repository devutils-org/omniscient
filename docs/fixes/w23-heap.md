# The twenty-third wave, stream heap

The heap family's nine pairs, run by the shared runner. Branch
`feat/w23-heap`: `172386d` (the pairs, the parser module's doc, the table)
and this handoff.

## What landed

- `catalog/references/heap.yaml`: every pair has an `id`
  (`dhat-total-bytes`, `dhat-total-blocks`, `dhat-gmax`, `dhat-end`,
  `massif-peak`, `heaptrack-calls`, `heaptrack-peak`, `heaptrack-leaked`,
  `memleak-outstanding`); every `omn` argv writes `-o {out}`, so omn's JSON
  is read from `{out}/results.json` and never shares a stdout with the
  workload; the family's `<emit>.<field>` grammar is gone, replaced by
  dotted paths of the README's grammar:
  `results.0.outputs[emit=topk].series.0.total` / `.events` and
  `results.0.outputs[emit=summary].series.0.max` / `.last`. Each argv was
  checked against today's `omn` (its real output captured once by hand:
  `topk` keyed `task:<pid>` with `events` and `total`; `summary` with
  `count`, `min`, `max`, `mean`, `last`). All pairs stay `run: separate`.
- `tests/reference/heap.rs`: only its module doc changed (it cites the
  README, and says no `omn_parse` parser is needed: every value omn holds
  at one place).
- `docs/replaces/heap.md`: every row with a pair cites its id and the
  runner's numbers; the rows without one keep their `no pair:` reason.
- No harness to delete: this family never had one of its own beyond a
  scratch emulation that was not committed.
- Captures: `dhat.stderr`, `massif.out`, `heaptrack_print.txt` were already
  real output of this machine (twenty-second wave); unchanged.
  `memleak.format.txt` stays a format fixture (root, below).

## Each pair's outcome (shown)

`make`, then `cargo test --test reference pairs_heap -- --nocapture`,
2026-10-04, under the other streams' load:

| Pair | omn | tool | Outcome |
|---|---|---|---|
| `dhat-total-bytes` | 73,637,336 | 73,637,336 | holds (rel 0.001) |
| `dhat-total-blocks` | 3,186 | 3,186 | holds (rel 0.01) |
| `dhat-gmax` | 68,908,064 | 68,908,064 | holds (rel 0.01) |
| `dhat-end` | 4,605,465 | 4,605,465 | holds (rel 0.005) |
| `massif-peak` | 68,908,064 | 68,908,064 | holds (rel 0.01) |
| `heaptrack-calls` | 3,186 | 3,180 | holds (rel 0.01) |
| `heaptrack-peak` | 68,908,064 | 68,980,000 | holds (rel 0.01) |
| `heaptrack-leaked` | 4,605,465 | 4,610,000 | holds (rel 0.01) |
| `memleak-outstanding` | -- | -- | skipped: `` `sudo -n` does not grant root here `` |

`tally: reference heap: 8 held, 1 skipped, 0 gaps, 0 known defects, 0 failed`.

The runner reads what the twenty-second wave's emulation read: the same
equalities to the byte against dhat and massif, the same 6-call and
72 KB offsets against heaptrack (its own in-process allocations, reasoned).
No pair needed a tolerance change.

## Shown vs reasoned

- **Shown**: the tally above; `cargo test --test reference -- heap::
  families_are_whole every_pair_is_well_formed
  every_row_names_a_pair_or_why_none`: the seven heap parser tests and
  `families_are_whole` pass; `every_pair_is_well_formed` and
  `every_row_names_a_pair_or_why_none` fail, naming no heap pair and no row
  of `docs/replaces/heap.md` (perfrecord's pairs, and a bpftrace row that
  points at this family's table -- theirs, phase 2).
  `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo fmt --check`: clean.
- **Reasoned**: the memleak pair (root). Its parser is checked against a
  format fixture, not the tool.

## Not done, and why

- The memleak capture and pair: `sudo -n` grants no root here.
- omn's gaps for this family (no stack on `mem.heap.alloc`, no eBPF
  collector, no high-water mark on `mem.heap.live`, no sum in `summary`, no
  live block count, no lifetimes) are unchanged and listed in
  `docs/fixes/w22-heap.md`; they are `src/`'s, the next waves'.

## Privileged runs wanted

```console
sudo -v   # by a person, once
make && cargo test --test reference pairs_heap -- --nocapture
sudo memleak -T 100000 -o 0 -c "python3 tests/reference/workloads/heap/blocks.py 64 1048576 4 0.3" 2 1 \
  > tests/reference/captures/heap/memleak.txt
```

Then add a parser test on `memleak.txt` beside the format fixture's.

## Decision lines

- Every heap pair writes `-o {out}`, even though `blocks.py` prints
  nothing: a workload that one day prints would otherwise land inside the
  JSON.
- The paths read series `.0`, not `.*`: the workload is one process, and a
  second series (a child) should fail the reading rather than be summed in
  silently.

## Needs from another stream

- **bpftrace**: its `docs/replaces/bpftrace.md` row "`memleak` (bcc):
  outstanding allocations" is not backed; it can cite this family's pair
  `memleak-outstanding` only by a pair of its own, or say `no pair:` (the
  heap family's pair covers it).
- **Coordinator**: nothing else; the full suite and `reference_tally` at
  the end.
