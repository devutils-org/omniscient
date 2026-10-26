# Fixes

One document per stream of the fix wave, the counterpart to `docs/scenarios/`.
Those recorded sixty-three defects; these record what became of each.

Each carries three things:

- **Fixed**, with the defect it closes and the scenario that now asserts the new
  behaviour. A fix whose scenario was not updated is not finished.
- **Something else**, for what turned out not to be the defect it was filed as.
  A wrong diagnosis corrected is worth as much as a fix.
- **Left**, with why. A machine that cannot exercise a fix, a change that needs
  another stream first, a defect that is really a decision.

See `docs/dev/briefs/wave-5-defects.md` for how the sixty-three were divided --
it was `docs/dev/BRIEF.md` while that wave ran -- and `docs/scenarios/<family>.md` for the
defects themselves.

## Closed later, by the tenth wave's coordinator

Four of the eleven left, each in a file no stream owned by then:

- **`sched` 7 — `--keys` was accepted, recorded, and not used.** Every reducing
  emit (`timeline`, `rate`, `summary`, `hist`, `topk`) now groups by the keys a
  request names, in the order asked, and an event without one of them is
  `<kind>:?` in that part rather than folded into a neighbour's series. With no
  keys asked for, a series is still the collector's first key.
  `emit::tests::the_keys_a_request_names_are_what_its_series_are_grouped_by`.
- **`mem.faults` did not say what it did not sample.** It now carries the same
  degradation as `cpu.profile` and `sched.switches`, with the count of tasks it
  sampled: a per-task sampling event is not inherited.
- **Two example sessions described a defect that is gone.** The headers of
  `examples/proc-one-reading.yml` and `examples/proc-exit.yml` now describe
  what the sessions show, and were checked against a run: `comm=python3` and
  three descriptors at the first instant, and a run that stops at the exit.
- **`io` 9 and `io` 7** had been closed in between: the loss headline names all
  five counters, and `io.bytes` declares `raw`.

Still left, and why: `sched` 5 (`count` refusing `topk`) and the runner's pace
and the load-sensitive spending test are in `src/request.rs` and
`src/collect/mod.rs`, which the tenth wave's hot attach stream owns; the manual's
exit codes and `=~` are in `man/omn.1`, which its triage stream owns. Both are
filed with them in `docs/open-findings.md`. `net` 3 is a catalog decision
recorded in `catalog.md`, not a defect.
