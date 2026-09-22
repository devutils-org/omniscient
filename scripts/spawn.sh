#!/usr/bin/env bash
# Prepares an isolated worktree for one work stream and prints its prompt.
#
#   scripts/spawn.sh cross-memory
#   cd ../omniscient-cross-memory && claude     # paste the prompt it printed
#
# Streams own disjoint sets of files (see BRIEF.md), so the worktrees never need
# to rebase against each other and the merge order does not matter. A merge that
# conflicts anywhere other than an append seam means a stream edited something
# it does not own.
#
# This wave's four crossings touch no file under src/ at all, which is what lets
# the two engine streams run beside them. The two session indexes --
# examples/README.md and GUIDE.md -- belong to the coordinator: every crossing
# writes sessions and none of them lists its own, because after the sixth wave
# both indexes were twenty-eight sessions short for exactly that reason.
set -euo pipefail

declare -A OWNS=(
    [cross-memory]="examples/cross-memory-*.yml tests/cross_memory.rs"
    [cross-compute]="examples/cross-compute-*.yml tests/cross_compute.rs"
    [cross-io]="examples/cross-io-*.yml tests/cross_io.rs"
    [cross-machine]="examples/cross-machine-*.yml tests/cross_machine.rs"
    [shared-layer]="src/bpf/attach.rs src/bpf/btf.rs src/bpf/insn.rs src/bpf/map.rs src/bpf/syscall.rs src/bpf/mod.rs catalog/*.yaml"
    [emit-short]="src/emit.rs src/encode.rs tests/aggregation.rs tests/encoders.rs"
)
declare -A TITLE=(
    [cross-memory]="Stream P1 -- memory, end to end"
    [cross-compute]="Stream P2 -- compute, end to end, and where phase 10 starts"
    [cross-io]="Stream P3 -- the I/O path"
    [cross-machine]="Stream P4 -- crossed families, and the machine as a confounder"
    [shared-layer]="Stream S -- the shared layer nobody owns"
    [emit-short]="Stream E -- rate, topk and stack are short in the same way"
)
declare -A LOG=(
    [cross-memory]="docs/scenarios/cross-memory.md"
    [cross-compute]="docs/scenarios/cross-compute.md"
    [cross-io]="docs/scenarios/cross-io.md"
    [cross-machine]="docs/scenarios/cross-machine.md"
    [shared-layer]="docs/families/shared-layer.md"
    [emit-short]="docs/families/emit.md"
)

CROSSING_WORK='Every source your crossing joins already collects and is already checked against
ground truth -- so nothing here is new mechanism, and that is the point. What is
new is the arithmetic that puts two families in one table, and the honesty about
what that table is missing.

You own no file under src/. If expressing your crossing needs a change in the
engine, that is a request under "## Needs from another stream" in
docs/open-findings.md, with the exact signature you want, under a heading naming
your stream -- not an edit. Nor do you edit examples/README.md or GUIDE.md: the
two session indexes belong to the coordinator, who regenerates them at the merge.

Start from examples/allocation-cost.yml, which is a join that works today, and
from docs/scenarios/<family>.md for what each workload guarantees -- the scenario
wave wrote a "## Workloads" section into every one of them for this wave.

**A crossing is landed when it says what evidence it has**: IDENTITY > KEY >
WAKEUP > TEMPORAL, on every joined result. A correlation backed by a propagated
identity and one backed by two things happening at the same time are both
correlations, and only one of them is evidence. And a decomposition whose parts
do not sum to the whole is hiding a state: print the residue rather than
rounding it away.'

SHARED_WORK='Read every "## Needs from another stream" section in docs/open-findings.md before
you write anything, and group them: three are asked for twice by two families
each, and the whole value of this stream is that they are written once. That is
what the second wave learned and what the third paid for by not doing.

**A request is landed with the collector that wanted it, or it is not landed.**
A helper added with no caller is a signature invented from a description. Where
the caller lives in a file you do not own, say so in your log and leave the
request in place rather than reaching into another stream.

Where you change a catalog entry requirements, re-probe: a source reported
available that would then fail at attach is finding 15 all over again, and it
cost this project thirty sources reading available once already. Say what moved,
in numbers, in your log.

Do not start fentry. It is the next wave, it is real mechanism, and this stream
is valuable because it is boring and finite.'

EMIT_WORK='Finding 37 was scoped to hist, summary and percentiles because three documents
had agreed that scope; stream F wrote the argument for widening it on its way
out, at the end of docs/open-findings.md. Shortfall::of is public and takes
(&Losses, records, per_record), so there is no new derivation to do.

What there is, is a decision: Output::Stack already carries unattributed and
Output::TopK already carries Rest, and each is that emit own statement about
what it does not account for. A mark bolted beside either is a second such
statement in different words. Make them one statement or keep them visibly
separate, and write down which you chose and why.

Your second file is src/encode.rs, not src/render.rs -- render.rs renders
requests, listings and refusals, and hist_text is in encode.rs. format.rs
declares what each format can carry and a test crosses that claim against
reality, so every format that can carry a topk or a stack carries the mark too,
or says it cannot.

And the negative matters as much as the positive: **a clean run must gain
nothing**, or the mark becomes noise and readers learn to ignore it.'

stream="${1:-}"
if [[ -z "$stream" || -z "${OWNS[$stream]:-}" ]]; then
    echo "usage: $0 <stream>" >&2
    echo "streams: ${!OWNS[*]}" >&2
    exit 2
fi

repo="$(git rev-parse --show-toplevel)"
branch="feat/${stream}"
worktree="${repo}/../omniscient-${stream}"

if [[ -d "$worktree" ]]; then
    echo "worktree already exists: $worktree" >&2
else
    git -C "$repo" worktree add -b "$branch" "$worktree" HEAD >&2
fi

case "$stream" in
    shared-layer) WORK="$SHARED_WORK" ;;
    emit-short) WORK="$EMIT_WORK" ;;
    *) WORK="$CROSSING_WORK" ;;
esac

cat <<PROMPT

--- worktree ready: $worktree (branch $branch) ---
--- start the agent there, and give it this prompt: ---

You are ${TITLE[$stream]} of the Omniscient project.

Read these first, in this order:
  1. CLAUDE.md                 the standing invariants of this repository
  2. STATE.md                  what works today, measured, and what is left
  3. BRIEF.md                  the rules, then the section for your stream
  4. docs/contributing.md      what ground truth means here

You own, and may only edit:
  ${OWNS[$stream]}
  ${LOG[$stream]}    -- your handoff, and the deliverable rather than a log about one

Everything else is read-only. Needing something from another stream files is not
a reason to edit them: write it under "## Needs from another stream" in
docs/open-findings.md, under a heading naming your stream, with the exact
signature you want, and carry on. That file is append-only per stream -- wave
three had six streams edit it freely and produced six conflicts on it.

Build with \`make\`, not \`cargo build\`: the preload shim is a workspace member
nothing depends on, and the CAP_BPF and CAP_PERFMON file capabilities do not
survive a relink. Both failures are silent -- the tool reports fewer sources as
available, which reads like the machine changed rather than the binary.

${WORK}

You are done when \`make check\` is clean and everything you were given either
works with its scenarios, or is explained in ${LOG[$stream]}.

Ground truth is the bar: a test that checks the code returns what the code
computed proves nothing. Choose a workload whose property you know independently
-- a known number of I/Os, an injected delay, a program that reports its own
state -- and check the tool against that.

A test that skips is not a test that passes, and a test that asserts what the
machine was doing is worse than either. This wave is more exposed to that than
any before it: a crossing is two sources on a shared machine at the same time.
Assert the separation rather than a calibrated threshold, carry the reference
own resolution as slack, and where the property cannot be created here, skip
loudly with the numbers that show why, and say so in ${LOG[$stream]}.

Commit on ${branch}. Do not merge; the coordinator does that.

PROMPT
