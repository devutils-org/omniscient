# Stream lazy, the twenty-fifth wave

Branch `feat/w25-lazy`. Done sequentially by one agent.

## Outside the owned files, said first

- `src/main.rs` (run's): `list`'s JSON gains `defaults` per entry and
  `emits` per backend probe; `trace` calls `omn::lazy::keep_band` right
  after resolution, before the launch and budget checks.

## What landed

1. **`omn list --format json` carries the defaults and each backend's
   emits**, without changing its shape: every entry gains `defaults` (the
   band and the request its family's default writes, absent when in none),
   every `probes[]` item `emits` (its own when declared, the entry's
   otherwise).
2. **`omn-mcp` offers the defaults**: `plan_session` and `run_session` take
   `family`, `cost`, `pid`, `cgroup`, `system`, `duration`, `program` in
   place of a session, composed through `omn::lazy::compose_words` (new: the
   words of an `omn trace` line, parsed as the command parses them). The
   answer's `composed` holds the `note:` lines. A composed launch is still
   refused by `run_session` (only approved sessions launch).
3. **The band chooses the backend** (`omn::lazy::keep_band`): a composed
   request whose first viable backend declares more than the band is moved to
   the first later backend that declares the band or less and resolves here,
   and the run says so with what it gives up. What still cannot keep the band
   is said by `band_notes`, as before.
4. **Defaults**: `net.tcp.lifetime` in net's `med`; `proc.tasks` beside
   `proc.threads` in proc's `low`.

## Shown vs reasoned

- **The band's backend** (shown): `omn trace mem --cost low --pid <sleep>`
  is served by `fs:proc:statm` and read 2.0 MiB; `/proc/<pid>/status`'s
  `VmRSS` said 2012 kB. `--cost med` keeps `fs:proc:smaps_rollup`.
- **The MCP composition** (shown by test): the sources `plan_session`
  composes for `{family: mem, cost: low, pid}` are those `omn trace mem
  --cost low --pid N --dump-request` writes.
- **`cpu.profile` (`pmu:sw`) and the gpu sources at `--cost low`**: their
  chains have no later backend at `low`, so they are still said, not moved.
- `net.tcp.lifetime` in the default: its backends need CAP_BPF, so the net
  default was not run here.

## Found, not fixed

- **`omn list --format toml` fails** ("unsupported array type"): a TOML
  document cannot be a top-level array. Same on `main` before this stream.
  Either wrap the listing in a table for TOML or refuse `toml` for `list`
  with the reason (run's `src/main.rs`).

## Tests

`--test lazy` (2 new), `-p omn-mcp` (1 new), `--test cli --test triage`,
`--lib lazy catalog`, clippy, fmt.

## Decision lines for `docs/DECISIONS.md`

- A composed default's band chooses the backend where its chain has a later
  one that keeps the band; the move is said with what it degrades. A pinned
  request is never moved.
- `omn-mcp` composes a family's default through the same parser as the
  command line (`omn::lazy::compose_words`), so the two cannot drift.
