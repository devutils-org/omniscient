# Stream io, the twenty-fifth wave

Branch `feat/w25-io`. Done sequentially by one agent.

## What landed

1. **Defect: `io.bytes` on `fs:proc` counted bytes from before the window**
   (top, finding 2). At the first tick, a baseline older than one poll
   interval is retaken; a younger one (a launched program's, taken just
   before release) is kept, so the release's bytes stay in the first
   interval.
2. **`io.vfs --keys op`**: one event per function per interval (or per
   counted window), `op` on each; `op` is in the entry's `keys:`.

## Shown vs reasoned

- The baseline (unit test with a fake `/proc/diskstats`: 8 sectors inside
  the window counted, the startup's million not). Not shown live: on this
  machine, unprivileged, `omn top` attaches no eBPF program and starts in
  under 100 ms, so the stale baseline does not occur here.
- `io.vfs -k op` (reasoned): validates and resolves; the program needs
  CAP_BPF. Wanted: `omn trace io.vfs -m count -k op -e rate --system -d 2s`
  beside `vfsstat`.

## Added after the wave's first pass

3. **A `kworker`-dispatched block request charged to its submitter**: a
   third program, on `block_rq_insert`, stamps who inserted each request in
   an LRU map; `io.latency`'s issue program reads the stamp before it
   compares the target and records the inserter in place of the kworker.
   Reasoned: the program builds against this kernel and an instruction test
   checks the stamp is taken before the comparison. Wanted: the system-wide
   run that showed `kworker/13:1H` issuing other programs' writes, rerun
   (`omn trace io.latency -e raw --system -d 10s` beside a buffered `dd ...
   conv=fsync`), every write charged to `dd`.

## Not done (as it stood before)

1. **A `kworker`-dispatched block request charged to the bio's submitter**:
   a `block_rq_insert` program carrying the submitter to the issue, in the
   hot path, that this machine can neither load nor verify, with no workload
   that shows the loss on a launched program (twelve runs attributed 32 of
   32 writes). Ranked first for the next wave with privileges.

## Needs from another stream

- **net**: `net.bytes` on `fs:proc` may take its baseline the same way;
  worth the same check.

## Tests

`--lib collect::io` (1 new), `--test family_io --test scenario_io`, clippy,
fmt.
