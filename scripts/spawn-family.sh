#!/usr/bin/env bash
# Prepares an isolated worktree for one family thread and prints its prompt.
#
#   scripts/spawn-family.sh cpu
#   cd ../omniscient-cpu && claude
#
# Each family owns a disjoint set of files (see docs/contributing.md), so the
# worktrees never need to rebase against each other and the merge order does
# not matter. See docs/contributing.md for what a family owns.
set -euo pipefail

FAMILIES="cpu mem io net sched power gpu proc runtime"
family="${1:-}"

if [[ -z "$family" ]] || ! grep -qw -- "$family" <<<"$FAMILIES"; then
    echo "usage: $0 <family>" >&2
    echo "families: $FAMILIES" >&2
    exit 2
fi

repo="$(git rev-parse --show-toplevel)"
branch="feat/${family}-collectors"
worktree="${repo}/../omniscient-${family}"

if [[ -d "$worktree" ]]; then
    echo "worktree already exists: $worktree" >&2
else
    git -C "$repo" worktree add -b "$branch" "$worktree" HEAD >&2
    mkdir -p "$worktree/src/collect/${family}" "$worktree/docs/families"
fi

# Each worktree builds into its own target directory. Sharing one would make
# the builds serialize on cargo's lock, which defeats the point of running them
# in parallel.
cat <<PROMPT

--- worktree ready: $worktree (branch $branch) ---
--- start the agent there, and give it this prompt: ---

You are implementing the \`${family}\` family of the Omniscient catalog.

Read these first, in this order:
  1. CLAUDE.md               the standing invariants of this repository
  2. docs/contributing.md    what a family owns, and what ground truth means
  3. STATE.md                where the project is and what is left
  4. CATALOG.md section for \`${family}\`, and catalog/${family}.yaml

You own, and may only edit:
  catalog/${family}.yaml
  src/collect/${family}/**
  tests/family_${family}.rs
  examples/${family}-*.yml
  docs/families/${family}.md

Everything else in the repository is read-only to you. If you need something
from the shared layer -- a probe class, an emit, a change to the perf or eBPF
layer -- do not edit it. Write the request under a "## Needs from the shared
layer" heading in docs/families/${family}.md, with the exact signature you want
and why the family cannot proceed without it, then carry on with what is not
blocked.

A source is done when its catalog entry is truthful, a collector exists for its
first viable backend, \`omn list ${family}\` reflects what this machine can
serve, losses are counted and surfaced, and a ground-truth test checks the
measurement against something independently known -- a kernel counter, an
injected delay, a known size. A test that checks the code returns what the code
computed proves nothing.

Before you finish: cargo test && cargo clippy --all-targets && cargo fmt --check

Commit on ${branch}. Do not merge; the coordinator does that.

PROMPT
