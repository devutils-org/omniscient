#!/usr/bin/env bash
# Prepares an isolated worktree for one work stream and prints its prompt.
#
#   scripts/spawn.sh bpf-attach
#   cd ../omniscient-bpf-attach && claude      # paste the prompt it printed
#
# Streams own disjoint sets of files (see BRIEF.md), so the worktrees never need
# to rebase against each other and the merge order does not matter. A merge that
# conflicts anywhere other than a family's registry line means a stream edited
# something it does not own.
set -euo pipefail

declare -A OWNS=(
    [bpf-attach]="src/bpf/** src/collect/sched/**"
    [mem-breadth]="src/collect/mem/**"
    [counters]="src/perf.rs src/collect/cpu/**"
    [encoders]="src/encode.rs src/format.rs"
    [budget]="src/collect/mod.rs src/main.rs"
    [net-breadth]="src/collect/net/**"
)
declare -A TITLE=(
    [bpf-attach]="Stream A -- eBPF attach kinds"
    [mem-breadth]="Stream B -- memory breadth"
    [counters]="Stream C -- hardware counters and per-cpu perf"
    [encoders]="Stream D -- profile and trace encoders"
    [budget]="Stream E -- cost, budget and the self-benchmark"
    [net-breadth]="Stream F -- network breadth"
)
declare -A LOG=(
    [bpf-attach]="LOG_A.md"
    [mem-breadth]="LOG_B.md"
    [counters]="LOG_C.md"
    [encoders]="LOG_D.md"
    [budget]="LOG_E.md"
    [net-breadth]="LOG_F.md"
)

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
  plus tests for what you add, the entries of your own sources in catalog/*.yaml,
  docs/open-findings.md, and ${LOG[$stream]}

Everything else is read-only to you. Needing something from another stream's
files is not a reason to edit them: write the request under a "## Needs from
another stream" heading in docs/open-findings.md, with the exact signature you
want and why, and carry on with what is unblocked.

Build with \`make\`, not \`cargo build\`: the preload shim is a workspace member
nothing depends on, and the CAP_BPF and CAP_PERFMON file capabilities do not
survive a relink. Both failures are silent -- the tool reports fewer sources as
available, which reads like the machine changed rather than the binary.

You are done when \`make check\` is clean, everything you added has a test that
would fail without it, and anything you discovered that belongs to another
stream is written down rather than fixed in passing.

Finish by writing ${LOG[$stream]} at the repository root: what you landed, what
you measured, what you found and left alone, and what the next person needs to
know that is not in the code. The coordinator reads that, not your diff.

Ground truth is the bar: a test that checks the code returns what the code
computed proves nothing. Check against something independently known -- a kernel
counter, an injected delay, a known distribution, a program that reports its own
state. And a test that skips is not a test that passes: a machine that cannot
serve a request and a request that cannot mean anything are different answers,
and only the first is a reason to stop quietly.

Commit on ${branch}. Do not merge; the coordinator does that.

PROMPT
