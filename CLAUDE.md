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
| `STATE.md` | what works today, measured, and what remains |
| `docs/contributing.md` | what a family owns, and what ground truth means |
| `GUIDE.md` | how to run the demonstrations, and what each session shows |
| `docs/how-to-use/` | the user's guide, per family: which source, target, mode and cost for which question |
| `bench/apps/` | the application suite: one manifest per archetype, run by `scripts/apps.py` (`make apps`) |
| `docs/scenarios/*.md` | per family: what is worth measuring, and the defects the scenarios found |
| `BRIEF.md` | the current wave of work, one section per stream; the only brief -- what comes after it is `STATE.md`'s "What remains" |
| `docs/briefs/` | every earlier wave's brief, because `BRIEF.md` is overwritten each time and dozens of documents cite it |
| `docs/open-findings.md` | findings that cut across families |
| `docs/distroproof.md` | every distribution, run with distroproof: the matrix, its findings, and their fixes (`ROADMAP.md` phase 13) |
| `man/omn.1` | the manual page |

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

## How much to test, and when

The full suite takes 5 to 10 minutes and measures the real machine, so running
it by reflex is what makes a session slow. Run the smallest check that can
catch what the change could break:

- **While iterating**: only the tests of what changed — the touched module
  (`cargo test --lib <module>`) and the scenario file of that family
  (`cargo test --test scenario_<family>`). Not `make check`.
- **Once per commit**: one `make check`. If it goes red, rerun it with
  `cargo test --workspace --no-fail-fast` to see every failure in one pass,
  rather than fixing and restarting from the first one.
- **A failure already in the flaky tally** (`docs/open-findings.md`,
  `docs/scenarios/io.md`): rerun that one test alone, **once**. If it passes,
  move on. Do not rerun the whole suite for it. Only a test that fails
  *repeatedly* is worth investigating.
- **A commit that touches only documents, indexes or sessions**: run
  `cargo test --test cli` (the index and session-validation tests), nothing
  else.
- **Keep the full suite for** changes to a shared layer (`src/bpf/insn.rs`,
  `btf.rs`, `map.rs`, `emit.rs`, `join.rs`, the catalog loader). Those are
  where a full run has actually caught defects.

### In a wave of parallel streams

Ten streams each running the full suite cost a wave about a hundred full runs,
each two to three times slower under the others' load, and the load itself
turns timing tests red. So in a wave everything is scoped:

- **A stream, its fix round and its reviewer test only what the stream
  produced**: the tests of its own files, plus `cargo clippy --workspace` and
  `cargo fmt --check`. No `make check`, no `cargo test --workspace` -- unless
  the stream changed a shared layer listed above.
- **A merge agent merges** and runs the tests of what a conflict touched; it
  does not run the full suite per phase.
- **The coordinator runs the full suite once**, at the end of the wave, when
  every stream is merged and the machine is quiet. A red test goes back as a
  fix; one in the flaky tally is rerun alone once.

`make` still has to run before any eBPF test, because a relink drops the
capabilities — but once per build, not once per test.

## Checks

```console
make          # build the workspace and grant the capabilities
make check    # the full suite, clippy and fmt, the way CI would
```

`make` rather than `cargo build` for two reasons, both of which are silent
failures otherwise.

`--workspace` is not optional: the preload shim is a workspace member that
nothing depends on, so a plain `cargo build` does not build it, and
`mem.heap.alloc` then resolves onto a backend with no collector.

And the eBPF and per-cpu perf backends need `CAP_BPF` and `CAP_PERFMON`, which
are file capabilities that **do not survive a relink** -- every `cargo build`
drops them. On the development machine that is the difference between 92 usable
sources and 56, and nothing announces it: the tool reports fewer things as
available, which reads like the machine changed rather than the binary. `make`
reapplies them, and says so when it cannot.

The capability step never prompts. `sudo -n` on purpose: a build that asks for
a password blocks forever wherever nobody is there to type one, and "nobody is
there" is not the same as "no terminal" -- a pty with no human behind it is what
a build hook, a CI runner and an agent shell all have. To stop needing sudo at
all, one narrowly scoped line under
`visudo -f /etc/sudoers.d/omniscient`, which permits exactly those commands on
the checkout and its per-stream worktrees and nothing else:

```
<user> ALL=(root) NOPASSWD: /usr/bin/setcap cap_bpf\,cap_perfmon+ep /path/to/omniscient*/target/debug/omn, /usr/bin/setcap cap_bpf\,cap_perfmon+ep /path/to/omniscient*/target/release/omn
```

The capabilities go on `omn` alone, debug and release builds: `omn-mcp`
collects through the `omn` beside it and needs no line of its own
(`DECISIONS.md`, "one privilege model").

The comma is escaped and the path absolute on purpose (`docs/open-findings.md`
finding 10); `make` tries that spelling first.

A green suite on a machine without the capabilities is not evidence about the
eBPF layer: those tests skip, loudly. The integration tests run
`target/debug/omn` and do take the privileged path once it carries them; the
in-process unit tests cannot, because their binaries are rebuilt under a fresh
hash every run.
