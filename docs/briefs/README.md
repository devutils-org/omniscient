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
| 6 | `wave-6-collectors.md` | family, plus one layer stream | 22 of 24 sources; `scoped_eq` got its first producer; finding 37 closed |
| 7 | `wave-7-crossings.md` | question, not family: four crossings plus a shared-layer stream | four crossings, 27 scenarios, no file under `src/` touched by any of them; the sixth wave's twenty requests landed once |
| 8 | `wave-8-engine.md` | layer: the join engine, the run model, and the two attach kinds left | 89 sources from 81, `between` and a decomposition whose residue sits inside its own resolution; one cross-file change that three programs without a layout test could not refuse |
| 9 | `wave-9-vertical.md` | the milestone plus breadth: the vertical case, and three streams on six sources nothing blocks | phase 4 done within what needs no root; 96 of 101 sources; the first wave held to a testing rule, and the first merge green on its first run |

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

And one the sixth wave added: **name the seam, or it gets discovered.** That
wave's brief listed the shared eBPF files and omitted `src/bpf/mod.rs`, which is
where a program registers its `mod` and its `pub use` — so five streams edited
it. It cost nothing, because every edit was an append and four auto-merged, but
that was luck rather than design. Finding 39 has it.
