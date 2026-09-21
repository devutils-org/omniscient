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
| `docs/scenarios/*.md` | per family: what is worth measuring, and the defects the scenarios found |
| `BRIEF.md` | the current wave of work, one section per stream |
| `BRIEF_NEXT.md` | the wave after it, written early so its shape is settled |
| `docs/open-findings.md` | findings that cut across families |
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
drops them. On the development machine that is the difference between 62 usable
sources and 51, and nothing announces it: the tool reports fewer things as
available, which reads like the machine changed rather than the binary. `make`
reapplies them, and says so when it cannot.

The capability step never prompts. `sudo -n` on purpose: a build that asks for
a password blocks forever wherever nobody is there to type one, and "nobody is
there" is not the same as "no terminal" -- a pty with no human behind it is what
a build hook, a CI runner and an agent shell all have. To stop needing sudo at
all, one narrowly scoped line under
`visudo -f /etc/sudoers.d/omniscient`, which permits exactly that command on
exactly that path and nothing else:

```
<user> ALL=(root) NOPASSWD: /usr/sbin/setcap cap_bpf,cap_perfmon+ep /path/to/omniscient/target/debug/omn
```

A green suite on a machine without the capabilities is not evidence about the
eBPF layer: those tests skip, loudly. The integration tests run
`target/debug/omn` and do take the privileged path once it carries them; the
in-process unit tests cannot, because their binaries are rebuilt under a fresh
hash every run.
