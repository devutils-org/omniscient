# The briefs, wave by wave

`BRIEF.md` at the root is the **current** wave, and it is overwritten each time a
new one is dispatched. That is a problem this directory exists to fix: dozens of
documents and tests cite `BRIEF.md` for something a later wave replaced. Fifteen
of them say "the four classes of `BRIEF.md`", which are the scenario wave's and
have not been in that file since the wave after it.

So every wave's brief is kept here, recovered from the history where it had
already been overwritten. **A citation of `BRIEF.md` in a document older than the
current wave means the brief of the wave that document belongs to**, and the
table below says which.

| Wave | Brief | Divided by | What it produced |
|---|---|---|---|
| 1 | `wave-1-families.md` | source family | the first collectors, one directory each; zero conflicts |
| 2 | `wave-2-shared-layer.md` | shared layer | the eBPF layer, the emit layer, the join engine; a few conflicts, all in the one place two streams touched |
| 3 | `wave-3-breadth.md` | what unlocks what | breadth, and the attach kinds; six streams appending to one findings file produced six conflicts on it |
| 4 | `wave-4-scenarios.md` | family, with a file-name prefix as the lock | **the four scenario classes**, 175 scenarios, 63 defects; zero conflicts |
| 5 | `wave-5-defects.md` | defect class | 52 of the 63 closed; `docs/fixes/` is its record |
| 6 | `../../BRIEF.md` | family, plus one layer stream | in flight |

The four scenario classes are in `wave-4-scenarios.md` and explained for a
reader in `GUIDE.md`. They are: the capability sweep, the backend comparison,
the loss path, and the refusal.

## What every wave has taught, and it is the same three things

`STATE.md` carries this under "How this parallelizes", and it is worth having
beside the briefs themselves:

1. **The unit of ownership is a file set nobody else writes.** A family
   directory, or a file-name prefix. Wave four used `examples/<family>-*.yml`,
   `tests/scenario_<family>.rs`, `docs/scenarios/<family>.md` and had zero
   conflicts across eight streams.
2. **The handoff is a file only that stream can write.** "Write a log at the
   end" produced no logs in wave three; `docs/fixes/` and `docs/scenarios/`
   worked, because the document *was* the deliverable.
3. **Registries are append seams and belong per family.** A central `match` is
   one conflict per agent.
