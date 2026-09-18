# Adding a source

A source is an entry in the catalog and a collector that serves one of its
backends. Nothing else changes: the CLI, the validator, `omn list` and the help
are projections of the catalog, so a source that is described correctly appears
everywhere on its own.

## What you own

```
catalog/<family>.yaml          the entry
src/collect/<family>/**        the collector, and its `factories()` line
tests/family_<family>.rs       the ground truth
examples/<family>-*.yml        a session someone would actually run
docs/families/<family>.md      what the measurements taught
```

Nothing outside that list needs to change for a new source. If something does —
a probe class for a requirement nothing answers yet, an emit that does not
exist, a key the model has no variant for — that is a gap in the shared layer
and is worth writing down in `docs/open-findings.md` rather than worked around
locally.

The shared layer is `src/{catalog,request,resolve,validate,emit,encode,join,
perf,capabilities}.rs`, `src/bpf/`, and `src/collect/mod.rs`. Changing it is
fine; doing it without noticing that three families depend on the behaviour is
not.

## A catalog entry

The schema is in `CATALOG.md` §1. Four things are easy to get wrong:

**The backend chain is ordered and the first viable one wins.** Put the backend
that answers the question best first, not the one most likely to be available:
the resolver handles availability and reports what it fell back to.

**Every fallback either says what it degrades or declares `equivalent: true`.**
The loader refuses an entry that does neither. A degradation nobody is told
about is a wrong number that looks right; a vendor path that loses nothing is
not a degradation and saying it is would be noise.

**Backend ids are unique within a chain.** Two backends called `fs:proc` cannot
be told apart by `--backend force:`, and a collector cannot know which it is
serving.

**Caveats say what the number does not mean.** They reach the user with the
request, not in documentation nobody reads at the moment it matters. The good
ones are specific: a fault count is not a quantity of memory, a descriptor count
without its limit is not a measurement, `io.latency --pid` is reliable for reads
only.

## A collector

Register it from your family's `factories()`; there is no central match to edit.

```rust
Factory::new("net.bytes", "fs:proc", |_| Ok(Box::new(dev::Prepare(Counted::Bytes)))),
```

Two phases, because some collectors act on the target before it exists: a
preloaded shim is given at exec and cannot be handed to a running process.
`Prepared::launch_environment` contributes to the launch, `attach` binds once
the target exists, and `attach` takes the whole target rather than a pid because
an interface and a machine have none.

Refuse a target you cannot serve, by name. "Resident size is read per process,
and a system target names none" beats a zero.

## Ground truth is the bar

A source is done when a test checks its measurement against something
**independently known**: a kernel counter in `/proc`, an injected delay, a known
allocation size, a fixed iteration count, a program that reports its own state.

A test that checks the code returns what the code computed proves nothing, and a
collector returning a plausible constant passes every other kind. Examples in
this repository:

- `mem.faults` against the kernel's own minor-fault counter for the same process
- `proc.fd` against a launched program that counts its own descriptors
- `net.bytes` against 16 MiB pushed through loopback
- `net.backlog` against a one-deep accept queue given 64 connections
- `cpu.instructions` against a loop whose instruction count is known

Two traps, both of which have cost time here:

**A test that skips is not a test that passes.** A machine that cannot serve a
request and a request that cannot mean anything are different answers, and only
the first is a reason to stop quietly. A test that skipped on any failure passed
in green for hours while asking for an emit its source did not declare.

**`cargo test` runs its tests in parallel threads of one process.** Anything
measuring a process-wide count — descriptors, threads, context switches — cannot
get an exact delta there. Measure a launched process instead.

## The invariants

In `CLAUDE.md`, enforced by tests or by the catalog loader. If a change fights
one of them, the change is probably wrong.

## Before you finish

```console
make check
```

and confirm `omn list <family>` says what you expect on a machine that can serve
it and on one that cannot.
