# The briefs, wave by wave

`BRIEF.md` at the root is the **current** wave, and it is overwritten each time a
new one is dispatched. It is the only brief: what comes after it is written in
`STATE.md` under "What remains", not in a second brief, and a wave's brief is
copied here when that wave closes. That is a problem this directory exists to fix: dozens of
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
| 10 | `wave-10-depth.md` | roadmap task: the last of phases 4, 6 and 8, and phase 10 up to the escalation | the network rows injected without root, hot attach, the bench harness (run pending on a quiet machine), triage above the engine; the coordinator closed the ninth wave's requests and the kernel's skipped runs on `main` meanwhile, and the merge went green on its first full run |
| 11 | `wave-11-escalation.md` | milestone plus the debts the tenth wave filed, grouped by the files they land in; a fifth stream added mid-wave | the escalation (phase 10 done), the engine's debts, five silent degradations closed, drains that lose nothing, and the portability matrix: 22 cells on four kernels, ten classes of "available but does not attach" (P1-P10) |
| 12 | `wave-12-recorder.md` | the two milestones left, plus the matrix's findings split between the probes and the resolver, and the kernel programs nobody owned; the coordinator spawned the streams itself for the first time | the flight recorder, phase 11 (`omn-mcp`), finding 48 closed, three more drains exact, P1-P10 answered (most verified on matrix cells), a filter placed in the kernel only where a program compares it; the flaky tally audited (`docs/flaky.md`) and every cross-stream request inventoried (`docs/open-requests.md`) |
| 13 | `wave-13-cost.md` | the debts, grouped by the files they land in, with no milestone left; a seventh item added mid-wave by the user (`omn-mcp --allow-launch`); the matrix held back and swept alone after the merges | finding 47 closed (the cost inside the target), P11 closed, cgroups served by the counting sources and every `sched` and `proc` program, the 5.15 and 6.6 shapes read, stacks demangled, two new match kinds, and an agent that may launch what its operator wrote; 1994 tests green and thirteen matrix cells passing at the merge |
| 14 | `wave-14-product.md` | the first product wave: what someone who is not the developer meets first -- the stock machine, a release, one use case end to end -- plus the thirteenth wave's filings grouped by file; two foundation commits (R's vocabulary, U's `StackMode`) merged to `main` first | a source usable only through a written backend, the CI on two cells, unwind modes as a chain (`fp`, `fp+leaf`, `dwarf` served), `--container` bound to its cgroup, a release that installs, and an agent benchmarking a program through `omn-mcp`; 2068 passed at the merge, the portability judge red on cells swept before the probe fix until the matrix is swept again |
| 15, 16 | none | the list `STATE.md` left, split by the files each item lands in | each stream's handoff is `docs/fixes/w15-*.md` and `docs/fixes/w16-*.md`; `STATE.md` keeps what each merged |
| 17 | `wave-17-programs.md` | ROADMAP phase 12 (three case streams: the format, then CPython and `http.server`, then durability, a compiler and a build), the sixteenth wave's engine and core filings by file, and a stream on the in-kernel histogram's cost | seven case directories (one held back), every check asked both ways, `omn latency` and `omn-mcp`; a perf map as a symbol source with each frame saying where its name came from; the network namespace in an interface's identity, `proc` carried, a stall row's last end, exact sums from kernel-built rows, the GPL-2.0 text; the block histogram's own program pair, its cost not yet measured; 2189 and 2191 passed at the two merge checks, and sixteen findings the cases show, filed against the triage, the budget and the sched collectors |
| 18 | `wave-18-findings.md` | what the programs found, the CI's two reds, and the seventeenth wave's items 3 to 11, split by the files each lands in: ten streams, then two that needed the first ten merged | the triage follows a wait into the child and names the busiest thread; `--symbols` on the triage and perf-map frames ranked first; one privilege model for `omn` and `omn-mcp`; a budget over the process tree and a stop that ends it; the 280 % profile found; the sched collectors follow the target's children and read the kernel's own wait; the block programs' statistics readable; a join rule's tier; 43 bands corrected and held to the table; one order for unbuilt and missing; seven cases, `gcc-shell` back, every cause's first suspect right both ways; 2271 passed at the first merge, `make check` red on one tool gap (`cpu.profile`'s per-ring throttles) |

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
