# `proc` — implementation notes

## Status

| Source | Backend | Ground truth |
|---|---|---|
| `proc.fd` | `fs:proc` | a launched process reports its own descriptor count; the collector must match it exactly |
| `proc.threads` | `fs:proc` | same, for threads |

Blocked on the eBPF layer: `proc.syscall`, `proc.exec`, `proc.exit`,
`proc.signals`.

## Findings

### These are the only counts in the catalog that can be checked exactly

Everything else in this tool is measured with skid, sampling, or a folio size in
the way. A process holding seventeen descriptors holds seventeen. So the tests
assert exact equality rather than a tolerance, and they compare two independent
readers of the same truth: the measured process counts its own descriptors and
threads and reports them, and the collector reads the same kernel directories
from outside.

### The exact test cannot live in the unit tests

`cargo test` runs tests in parallel threads of one process, so other tests open
files and start threads while a unit test tries to measure a delta. The first
attempt asserted "open seventeen files, the count rises by seventeen" against
this process and failed, correctly. The exact comparison moved to
`tests/family_proc.rs`, against a launched process that nothing else touches;
the unit tests keep the claims that survive a shared process.

### Reading your own descriptor table is not like reading someone else's

The handle opened to walk `/proc/<pid>/fd` appears in that listing when the pid
is your own, and does not when it is not. The first implementation subtracted it
unconditionally, which was right for the unit test measuring this process and
wrong by one for every real target. The exact ground truth against a child is
what caught it — a tolerance of plus or minus one would have hidden it forever.

### A count without its limit is not a measurement

900 open descriptors is comfortable against a limit of 524288 and an outage
against a limit of 1024. The soft limit and the headroom travel with the count,
and a test asserts all three are present.

## Needs from the shared layer

- The eBPF layer, for the other four sources. `proc.syscall` in particular needs
  in-kernel aggregation before it is usable: `raw_syscalls` fires on everything,
  and a raw trace of it on a syscall-heavy workload is a load test rather than a
  measurement.
- `proc.exec` and `proc.exit` want the `proc:pidns+pid` key rather than
  `task:tid`, since a tid alone is ambiguous the moment containers exist. The
  key exists in the model and nothing produces it yet.
