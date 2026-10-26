# w26 kinds — the backend kind as a type, and the runner naming events

Phase 0a of the twenty-sixth wave (`docs/dev/BRIEF.md`, "0a · `kinds`").

## Landed

- `catalog::BackendKind`, read once from a backend id (`BackendKind::of`,
  `Backend::kind`). The resolver asks `narrows_in_program()` where it tested
  `starts_with("bpf")` (a `bpf_only` target, the pushed-down filter credit);
  the runner asks `loads_programs()` and matches the kind in
  `InsideMeter::of` where it split the id on `:`.
- **The runner names every event** (`collect::stamp`, inside
  `into_our_ids`, the one funnel every sample and stop goes through):
  `<source>/<backend>` of the request the collector was built for,
  `CollectorId::of`. Collectors no longer name themselves:
  `Collector::id()` is gone, the `id` fields of about a hundred collectors
  with it, events are built with `CollectorId::unstamped()`, and eleven
  event helpers lost their `id` parameter.

## Shown

- `omn list --format json` byte-identical to `bd09424`'s on this machine.
- `cargo test --lib`: 1793 passed. `collect::tests::every_event_handed_over_is_named_after_the_pair_it_was_built_for`
  (a made-up name replaced by the pair), `catalog::tests::every_shipped_backend_is_of_a_known_kind`.
- `tests/family_cpu.rs::a_counting_source_samples_on_its_own_counter`, which
  holds each sampled event's `collector_id` to `<source>/<backend>` through
  the binary: green with the capabilities. `cargo test --test cli` green.

## Counts

| | before | after |
|---|---|---|
| `starts_with("bpf")` / `split(':')` on a backend id outside the catalog | 4 | 0 (tests still read ids) |
| `CollectorId::new` outside tests | 98 | 1 (`ctf.rs`: imported events, not a collector) |
| `fn id(&self) -> &CollectorId` | 107 | 0 |

## Not done

- The two id equality checks in `InsideMeter::of`
  (`fs:proc:clear_refs`, `fs:sys:page_idle`, `vendor:nvml`) stay: they are
  properties of one backend, not of a kind.
