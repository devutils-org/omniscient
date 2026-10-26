# Brief — a profile's functions compared across runs, by what their names allow

The twenty-eighth wave, from a request a campaign wrote (ZML's, 26 October
2026, `NOTE-OMN-COUT-FONCTIONS.md` beside the checkouts): from a versioned
session and two kept runs, answer *which functions cost most, which changed,
and with what confidence*. `cpu.profile` and `omn diff` existed and had
already found a real cost (`CopyThunk` in Qwen's decoding, a 16.5 % gain
confirmed without the collector); what the campaign's real profiles showed
was a restitution gap, not a profiler one:

- the verdict's first row was a kernel address `kptr_restrict` withheld;
- one libc function with no symbol was four rows, one per instruction;
- a JIT's code, unnamed, was a set of addresses that changed between
  processes, and read as functions that vanished and appeared;
- the confidence was a Poisson count of one run's samples, read as if it
  said something about two independent runs.

Run autonomously on 2026-10-26, in one worktree (`claude/w28`).

## What it built

1. **Identity** (`triage::diff::Identity`): symbol, offset, anonymous,
   withheld kernel address, unmapped. A row is comparable when it is a symbol,
   or an offset into a file both runs mapped in the same build -- which each
   run now records, `stack::Provenance::build_ids`, filled by `cpu.profile`.
   Only comparable rows are in the verdict; the others are summed by what they
   lack (`Diff::unnamed`), with what would name them and the share they hold.
2. **Runs, not one run** (`triage::diff::diff_runs`): a side is one run, a
   repeated run (`-r N`'s directory or `repeat.json`), or several
   (`--before`, `--after`). Two or more a side: Welch's standard error between
   the runs, floored at the counts' own noise (`Confidence::BetweenRuns`).
   `-r N` accepts a session whose requests emit stacks.
3. **What the numbers are**: each row's share beside its absolute weight, the
   on-cpu time its samples stand for (where the rate is known), the spread
   across runs, `few_samples` under ten a run (listed, not ranked), and a
   `denominator` sentence: on-cpu time summed over the target's threads, not
   wall clock, not GPU; inclusive costs do not add up; sampling is not a
   duration per call.
4. **Each run's frame**: its `--window`, from `resolved.json` (two windows
   said), and its collector's measured cost, from `cost.json`.

## What it measured

The fixture's `diff` mode, with a new `waits` that sleeps as long as each
function runs: `waits` is charged no cpu; `beta` doubled is the only
difference; stripped, `beta`'s offsets are the difference, compared by the
program's build id; three runs a side are weighed between runs. On ZML's two
Qwen profiles, the verdict moved from `0xffffffffba9162b7` to
`ynn::dot_bf16_bf16_fp32_5x16x1...`, and 85.6 % / 80.9 % of the samples are
said to be code with no stable name (37.7 % withheld kernel, 34.7 % JIT, 13.2 %
libc offsets with no build id in those older runs).

## Left, and why

A duration per call is another source (USDT, uprobes, spans), with its own
recursion, threads, lost calls and cost: not `cpu.profile`'s. Grouping an
unsymbolized function's instructions into one row needs a symbol table the
file does not have: `--symbols debuginfod` is the road, and the note says so.
