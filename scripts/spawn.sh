#!/usr/bin/env bash
# Prepares an isolated worktree for one work stream and prints its prompt.
#
#   scripts/spawn.sh aggregation
#   cd ../omniscient-aggregation && claude      # paste the prompt it printed
#
# Streams own disjoint sets of files (see BRIEF.md), so the worktrees never need
# to rebase against each other and the merge order does not matter. A merge that
# conflicts means a stream edited something it does not own.
set -euo pipefail

declare -A OWNS=(
    [aggregation]="src/emit.rs src/encode.rs src/format.rs"
    [stacks]="src/stack.rs (new) and the callchain parts of src/perf.rs"
    [join]="src/join.rs"
    [catalog-model]="src/catalog.rs src/resolve.rs src/validate.rs src/event.rs src/request.rs, and the targets:/filter_fields:/keys: fields of catalog/*.yaml"
    [bpf]="src/bpf/** src/collect/sched/**"
    [small]="src/collect/mod.rs src/main.rs src/capabilities.rs src/cli.rs"
)
declare -A STREAM=(
    [aggregation]="A"
    [stacks]="B"
    [join]="C"
    [catalog-model]="D"
    [bpf]="E"
    [small]="F"
)
declare -A TITLE=(
    [aggregation]="Stream A -- aggregation and presentation"
    [stacks]="Stream B -- stack capture"
    [join]="Stream C -- the join engine"
    [catalog-model]="Stream D -- the catalog model"
    [bpf]="Stream E -- eBPF attachment"
    [small]="Stream F -- the honest small ones"
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
  plus tests for what you add, and docs/open-findings.md

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

Ground truth is the bar: a test that checks the code returns what the code
computed proves nothing. Check against something independently known -- a kernel
counter, an injected delay, a known distribution, a program that reports its own
state. And a test that skips is not a test that passes: a machine that cannot
serve a request and a request that cannot mean anything are different answers,
and only the first is a reason to stop quietly.

Commit on ${branch}. Do not merge; the coordinator does that.

PROMPT
