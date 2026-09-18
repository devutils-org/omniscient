# Omniscient — working notes for agents

## Ground rules

- **Everything written here is in English**: code, identifiers, comments, doc
  comments, CLI help, error messages, catalog YAML and the Markdown documents.
  Conversation may be in French; the artifacts are not.
- **Never use CodSpeed skills, MCP tools or connectors.** This project is
  unrelated to CodSpeed. Commit with `git commit`, open pull requests with
  `gh pr create`.

## Where things live

| Path | What it is |
|---|---|
| `CATALOG.md` | the vocabulary: families, axes, backend notation, caveats |
| `catalog/*.yaml` | the same vocabulary as data, compiled into the binary |
| `ROADMAP.md` | milestones and their exit criteria, in order |
| `DECISIONS.md` | what is settled, what is still open, and why |

The catalog is **data, not documentation**. The CLI, `omn list`, the validator
and the resolver are projections of it: adding a source means adding an entry,
never touching code.

## Standing invariants

These are enforced by tests or by the catalog loader; if a change fights one of
them, the change is probably wrong.

- A probe answers available, missing or **unknown**, and unknown never counts as
  available.
- A fallback backend either states what it degrades or declares
  `equivalent: true`.
- Backend ids are unique within a chain.
- `CLOCK_MONOTONIC` is the only clock domain; timestamps from elsewhere carry a
  non-zero `ts_err_ns`.
- An invalid combination is refused with the reason, never degraded silently.
- Losses are counted and surfaced to the user, always.

## Checks

```console
cargo test && cargo clippy --all-targets && cargo fmt --check
```
